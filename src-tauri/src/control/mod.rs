//! Miner control: start (resume), stop (pause), reboot, fault light.

pub mod cache;
pub mod creds;
pub mod driver;
pub mod job;
pub mod overrides;
pub mod safety;
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
#[cfg(test)]
mod light_e2e_tests;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use driver::{Capabilities, DriverFactory, MinerAction, OverlayFactory};
use job::{ActionRowEvent, JobConfig};

pub const EVENT_ROW: &str = "control:row";
pub const EVENT_SAFETY_ROW: &str = "safety:row";
pub const EVENT_SAFETY_DONE: &str = "safety:done";
pub const EVENT_SAFETY_ALERT: &str = "safety:alert";
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
    /// Absent = the action class default (off for start/stop/reboot, on
    /// for every newer class such as the fault light).
    #[serde(default)]
    pub dry_run: Option<bool>,
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
    /// Per unsupported action: every path considered and why it was not usable.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub why: std::collections::BTreeMap<MinerAction, String>,
}

pub fn caps_row(ip: IpAddr, d: Option<&dyn driver::MinerDriver>) -> CapsRow {
    let Some(d) = d else {
        return CapsRow { ip: ip.to_string(), found: false, caps: Capabilities::none(), why: Default::default() };
    };
    let caps = d.caps();
    let why = MinerAction::ALL
        .into_iter()
        .filter(|a| !caps.supports(*a))
        .map(|a| (a, d.paths_tried(a).join("; ")))
        .collect();
    CapsRow { ip: ip.to_string(), found: true, caps, why }
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
        dry_run: req.dry_run.unwrap_or_else(|| safety::ActionKind::from(req.action).default_dry_run()),
        verify: req.verify.then(|| job::VerifyConfig {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(req.verify_timeout_sec.clamp(10, 900)),
        }),
        allow_unconfirmed: false,
    }
}

/// Audit file: `<app data dir>/audit/actions.jsonl`.
pub const AUDIT_FILE: &str = "audit/actions.jsonl";
/// Pre-write snapshots: `<app data dir>/audit/snapshots.jsonl`.
pub const SNAPSHOT_FILE: &str = "audit/snapshots.jsonl";

fn audit_sink(app: &AppHandle) -> Arc<dyn safety::AuditSink> {
    match app.path().app_data_dir() {
        Ok(dir) => Arc::new(safety::JsonlAudit::new(dir.join(AUDIT_FILE))),
        Err(e) => {
            eprintln!("audit: no app data dir ({e}); audit disabled for this run");
            Arc::new(safety::NullAudit)
        }
    }
}

#[tauri::command]
pub async fn miner_capabilities(ips: Vec<String>) -> Result<Vec<CapsRow>, String> {
    let ips = parse_ips(&ips)?;
    let f: Arc<dyn DriverFactory> = Arc::new(OverlayFactory::with_store(Duration::from_secs(5), &creds::KeyringStore));
    let resolver = cache::Resolver::new(f, cache::DeviceCache::global());
    let mut out = Vec::new();
    for ip in ips {
        let d = match resolver.resolve(ip, Duration::from_secs(5)).await {
            cache::Resolved::Ready { driver, .. } => Some(driver),
            _ => None,
        };
        out.push(caps_row(ip, d.as_deref()));
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
        let ctx = job::SafetyCtx { run_id: rid.clone(), audit: audit_sink(&app), cache: cache::DeviceCache::global() };
        let results = job::run_action_guarded(ips, cfg, factory, cancel, sink, ctx).await;
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

fn default_dry_run_true() -> bool {
    true
}
fn default_halt() -> f64 {
    0.2
}

/// Batch settings write (frequency / voltage / fan / power limit / tuning
/// percent). Dry run unless explicitly turned off. No backend is
/// hardware-confirmed yet, so live writes report `no_path`; dry runs show
/// the path that would be used.
#[derive(Debug, Deserialize)]
pub struct SettingsJobRequest {
    pub ips: Vec<String>,
    pub change: safety::settings::Settings,
    #[serde(default = "default_dry_run_true")]
    pub dry_run: bool,
    #[serde(default)]
    pub confirm: safety::Confirmation,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_stagger")]
    pub stagger_ms: u64,
    /// First-wave size (default = concurrency).
    #[serde(default)]
    pub canary_size: Option<usize>,
    #[serde(default = "default_halt")]
    pub halt_threshold: f64,
}

pub fn batch_options(req: &SettingsJobRequest, controls: Vec<IpAddr>) -> safety::batch::BatchOptions {
    safety::batch::BatchOptions {
        concurrency: req.concurrency.clamp(1, MAX_CONCURRENCY),
        stagger: Duration::from_millis(req.stagger_ms),
        canary_size: req.canary_size,
        halt_threshold: req.halt_threshold.clamp(0.0, 1.0),
        controls,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsDoneEvent {
    pub run_id: String,
    pub results: Vec<safety::WriteOutcome>,
}

/// Whether one settings field can be written on a device, and how.
#[derive(Debug, Clone, Serialize)]
pub struct FieldSupport {
    /// Backend that would take the write (`None` = not supported).
    pub path: Option<String>,
    /// False = the backend is not yet confirmed on real hardware (dry run only).
    pub confirmed: bool,
    /// Every path considered and why.
    pub tried: Vec<String>,
}

/// Power / tuning support and limits for one device (dialog display).
#[derive(Debug, Clone, Serialize)]
pub struct SettingsSupportRow {
    pub ip: String,
    pub found: bool,
    pub class: Option<safety::DeviceClass>,
    pub model: Option<String>,
    pub power_w: Option<safety::limits::Range>,
    pub tuning_pct: Option<safety::limits::Range>,
    pub power: FieldSupport,
    pub tuning: FieldSupport,
    pub fan_pct: Option<safety::limits::Range>,
    pub temp_target_c: Option<safety::limits::Range>,
    /// Manual fan %.
    pub fan: FieldSupport,
    /// Auto fan with a target temperature (or plain auto where the firmware
    /// has no target, e.g. stock Antminer).
    pub fan_auto: FieldSupport,
    /// Tuning targets (plan PR D): mode, hashrate, preset, scaling.
    pub tuning_modes: Vec<safety::settings::TuningMode>,
    pub tuning_hashrate_ghs: Option<safety::limits::Range>,
    pub scaling_step: Option<safety::limits::Range>,
    pub scaling_min: Option<safety::limits::Range>,
    /// Per allowed-or-not mode: the path for `tuning_mode: low` (the
    /// representative check; High is refused by the limits everywhere).
    pub mode: FieldSupport,
    pub hashrate: FieldSupport,
    pub preset: FieldSupport,
    pub scaling: FieldSupport,
    /// Presets the miner lists (read once, only where presets are supported).
    pub presets: Vec<String>,
}

pub async fn settings_support_row(engine: &safety::SafetyEngine, ip: IpAddr, driver: Option<Arc<dyn driver::MinerDriver>>) -> SettingsSupportRow {
    let none = || FieldSupport { path: None, confirmed: false, tried: Vec::new() };
    let Some(d) = driver else {
        return SettingsSupportRow {
            ip: ip.to_string(),
            found: false,
            class: None,
            model: None,
            power_w: None,
            tuning_pct: None,
            power: none(),
            tuning: none(),
            fan_pct: None,
            temp_target_c: None,
            fan: none(),
            fan_auto: none(),
            tuning_modes: Vec::new(),
            tuning_hashrate_ghs: None,
            scaling_step: None,
            scaling_min: None,
            mode: none(),
            hashrate: none(),
            preset: none(),
            scaling: none(),
            presets: Vec::new(),
        };
    };
    let t = safety::Target::new(ip, d);
    let field = |change: safety::settings::Settings| {
        let (b, tried) = engine.select(&t, &change, true);
        FieldSupport { confirmed: b.as_ref().is_some_and(|b| b.confirmed_for(&change)), path: b.map(|b| b.id().to_string()), tried }
    };
    let limits = engine.limits.lookup(&t.profile);
    use safety::settings::{Settings as S, TuningMode};
    let mode = field(S { tuning_mode: Some(TuningMode::Low), ..Default::default() });
    let hashrate = field(S { tuning_hashrate_ghs: Some(1), ..Default::default() });
    let preset = field(S { tuning_preset: Some(String::new()), ..Default::default() });
    let scaling = field(S { scaling_step: Some(1), scaling_min: Some(1), ..Default::default() });
    let mut presets = Vec::new();
    if preset.path.is_some() {
        for b in t.driver.settings_backends() {
            if b.accepts(&t.profile, &S { tuning_preset: Some(String::new()), ..Default::default() }).is_ok() {
                presets = tokio::time::timeout(Duration::from_secs(5), b.presets()).await.unwrap_or_default();
                break;
            }
        }
    }
    SettingsSupportRow {
        ip: ip.to_string(),
        found: true,
        class: Some(t.profile.class),
        model: Some(t.profile.model.clone()),
        power_w: limits.and_then(|e| e.power_w),
        tuning_pct: limits.and_then(|e| e.tuning_pct),
        power: field(safety::settings::Settings { power_limit_w: Some(1), ..Default::default() }),
        tuning: field(safety::settings::Settings { tuning_pct: Some(100), ..Default::default() }),
        fan_pct: limits.and_then(|e| e.fan_pct),
        temp_target_c: limits.and_then(|e| e.temp_target_c),
        fan: field(safety::settings::Settings { auto_fan: Some(false), fan_pct: Some(100), ..Default::default() }),
        fan_auto: {
            let with_target = field(safety::settings::Settings { auto_fan: Some(true), temp_target_c: Some(60), ..Default::default() });
            if with_target.path.is_some() {
                with_target
            } else {
                let plain = field(safety::settings::Settings { auto_fan: Some(true), ..Default::default() });
                if plain.path.is_some() { plain } else { with_target }
            }
        },
        tuning_modes: limits.and_then(|e| e.tuning_modes.clone()).unwrap_or_default(),
        tuning_hashrate_ghs: limits.and_then(|e| e.tuning_hashrate_ghs),
        scaling_step: limits.and_then(|e| e.scaling_step),
        scaling_min: limits.and_then(|e| e.scaling_min),
        mode,
        hashrate,
        preset,
        scaling,
        presets,
    }
}

#[tauri::command]
pub async fn settings_support(ips: Vec<String>) -> Result<Vec<SettingsSupportRow>, String> {
    let ips = parse_ips(&ips)?;
    let f: Arc<dyn DriverFactory> = Arc::new(OverlayFactory::with_store(Duration::from_secs(5), &creds::KeyringStore));
    let resolver = cache::Resolver::new(f, cache::DeviceCache::global());
    let engine = safety::SafetyEngine::new(
        Arc::new(safety::NullAudit),
        Arc::new(safety::snapshot::SnapshotStore::in_memory(1)),
        safety::SafetyOptions::default(),
    );
    let mut out = Vec::new();
    for ip in ips {
        let d = match resolver.resolve(ip, Duration::from_secs(5)).await {
            cache::Resolved::Ready { driver, .. } => Some(driver),
            _ => None,
        };
        out.push(settings_support_row(&engine, ip, d).await);
    }
    Ok(out)
}

#[tauri::command]
pub async fn run_settings_job(app: AppHandle, req: SettingsJobRequest) -> Result<String, String> {
    let ips = parse_ips(&req.ips)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let rid = run_id.clone();
    tauri::async_runtime::spawn(async move {
        let cache = cache::DeviceCache::global();
        let factory: Arc<dyn DriverFactory> =
            Arc::new(OverlayFactory::with_store(Duration::from_secs(10), &creds::KeyringStore));
        let resolver = Arc::new(cache::Resolver::new(factory, cache.clone()));
        let snapshots = match app.path().app_data_dir() {
            Ok(dir) => safety::snapshot::SnapshotStore::with_file(1000, dir.join(SNAPSHOT_FILE)),
            Err(_) => safety::snapshot::SnapshotStore::in_memory(1000),
        };
        let engine = Arc::new(safety::SafetyEngine::new(audit_sink(&app), Arc::new(snapshots), safety::SafetyOptions::default()));
        let opts = batch_options(&req, cache.others(&ips, 3));
        let wreq = safety::SafeWriteRequest { change: req.change.clone(), dry_run: req.dry_run, confirm: req.confirm.clone() };
        let app2 = app.clone();
        let sink: safety::batch::OutcomeSink = Arc::new(move |o| {
            let _ = app2.emit(EVENT_SAFETY_ROW, o);
        });
        let (results, alert) =
            safety::batch::run_settings_job(&rid, ips, wreq, resolver, engine, opts, CancellationToken::new(), sink).await;
        if let Some(a) = alert {
            let _ = app.emit(EVENT_SAFETY_ALERT, a);
        }
        let _ = app.emit(EVENT_SAFETY_DONE, SettingsDoneEvent { run_id: rid, results });
    });
    Ok(run_id)
}
