//! Tauri commands for the SSH fallbacks and pool divert (plan K2).
//! Events: `ssh:row` / `ssh:done`, `divert:row` / `divert:done`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use super::cache::{DeviceCache, Resolved, Resolver};
use super::driver::{DriverFactory, MinerAction, OverlayFactory};
use super::safety::divert::{self, DivertOp, DivertOutcome, DivertRequest};
use super::safety::engine::{SafetyEngine, SafetyOptions, Target};
use super::safety::snapshot::SnapshotStore;
use super::safety::{Confirmation, DeviceClass};
use super::ssh::plan::{supports_model, SshAction};
use super::ssh::store::{self, KeyringKv, SecretKv, SshAccessSummary, SshLogin, SshSecret};
use super::ssh::transport::RusshTransport;
use super::ssh::{SshOptions, SshOutcome, SshRequest, SshRunner, SshTarget};
use super::{audit_sink, creds, parse_ips, SNAPSHOT_FILE};

pub const EVENT_SSH_ROW: &str = "ssh:row";
pub const EVENT_SSH_DONE: &str = "ssh:done";
pub const EVENT_DIVERT_ROW: &str = "divert:row";
pub const EVENT_DIVERT_DONE: &str = "divert:done";

fn resolver(timeout: Duration) -> Resolver {
    let f: Arc<dyn DriverFactory> =
        Arc::new(OverlayFactory::with_store(timeout, &creds::KeyringStore));
    Resolver::new(f, DeviceCache::global())
}

/// SSH target from the cache (identity from discovery; nothing probed).
fn ssh_target(ip: IpAddr) -> Option<SshTarget> {
    let d = DeviceCache::global().get(ip)?;
    Some(SshTarget::new(
        &ip.to_string(),
        d.profile.class,
        &d.profile.model,
        &d.profile.firmware,
        d.identity.mac.clone(),
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct SshActionSupport {
    pub action: SshAction,
    pub supported: bool,
    pub reason: Option<String>,
    pub hw_confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SshSupportRow {
    pub ip: String,
    pub found: bool,
    pub class: Option<DeviceClass>,
    pub model: Option<String>,
    pub actions: Vec<SshActionSupport>,
    pub access: Option<SshAccessSummary>,
    /// Native stop / start present (divert not needed).
    pub native_stop: bool,
    pub native_start: bool,
    /// A divert's saved pools are on record (restore available).
    pub diverted: bool,
}

#[tauri::command]
pub async fn ssh_support(ips: Vec<String>) -> Result<Vec<SshSupportRow>, String> {
    let ips = parse_ips(&ips)?;
    let r = resolver(Duration::from_secs(5));
    let kv = KeyringKv;
    let mut out = Vec::new();
    for ip in ips {
        let caps = match r.resolve(ip, Duration::from_secs(5)).await {
            Resolved::Ready { driver, .. } => Some(driver.caps()),
            _ => None,
        };
        let Some(t) = ssh_target(ip) else {
            out.push(SshSupportRow {
                ip: ip.to_string(),
                found: false,
                class: None,
                model: None,
                actions: Vec::new(),
                access: None,
                native_stop: false,
                native_start: false,
                diverted: false,
            });
            continue;
        };
        let actions = SshAction::ALL
            .into_iter()
            .map(|a| match supports_model(t.class, &t.model, a) {
                Ok(sh) => SshActionSupport {
                    action: a,
                    supported: true,
                    reason: None,
                    hw_confirmed: sh.hw_confirmed,
                },
                Err(e) => SshActionSupport {
                    action: a,
                    supported: false,
                    reason: Some(e),
                    hw_confirmed: false,
                },
            })
            .collect();
        out.push(SshSupportRow {
            ip: ip.to_string(),
            found: true,
            class: Some(t.class),
            model: Some(t.model.clone()),
            actions,
            access: Some(store::summarize(&kv, t.class, &t.key())),
            native_stop: caps.as_ref().is_some_and(|c| c.supports(MinerAction::Stop)),
            native_start: caps
                .as_ref()
                .is_some_and(|c| c.supports(MinerAction::Start)),
            diverted: divert::saved(&kv, &ip.to_string()).ok().flatten().is_some(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct HostKeyInfo {
    pub ip: String,
    pub fingerprint: String,
    pub pinned: Option<String>,
    pub matches: bool,
}

/// Handshake only (no login, no command) to show the host key in the
/// trust prompt. Runs only when the operator opens the prompt.
#[tauri::command]
pub async fn ssh_fetch_host_key(ip: String) -> Result<HostKeyInfo, String> {
    let ip: IpAddr = ip.trim().parse().map_err(|_| format!("invalid IP: {ip}"))?;
    let t = ssh_target(ip).ok_or("miner not discovered yet")?;
    let runner = SshRunner {
        transport: Arc::new(RusshTransport::default()),
        kv: Arc::new(KeyringKv),
        audit: Arc::new(super::safety::NullAudit),
        opts: SshOptions::default(),
    };
    let (fingerprint, pinned) = runner.host_key(&t).await?;
    Ok(HostKeyInfo {
        ip: ip.to_string(),
        matches: pinned.as_deref() == Some(fingerprint.as_str()),
        fingerprint,
        pinned,
    })
}

/// Pin the fingerprint the operator just confirmed.
#[tauri::command]
pub async fn ssh_pin_host_key(ip: String, fingerprint: String) -> Result<(), String> {
    let ip: IpAddr = ip.trim().parse().map_err(|_| format!("invalid IP: {ip}"))?;
    let t = ssh_target(ip).ok_or("miner not discovered yet")?;
    let kv: &dyn SecretKv = &KeyringKv;
    kv.pin(&t.key(), fingerprint.trim())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ssh_unpin_host_key(ip: String) -> Result<(), String> {
    let ip: IpAddr = ip.trim().parse().map_err(|_| format!("invalid IP: {ip}"))?;
    let t = ssh_target(ip).ok_or("miner not discovered yet")?;
    let kv: &dyn SecretKv = &KeyringKv;
    kv.unpin(&t.key()).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct SshLoginInput {
    pub class: DeviceClass,
    /// Per-device override; absent = every miner of this firmware.
    #[serde(default)]
    pub ip: Option<String>,
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub private_key: Option<String>,
}

fn dev_key(class: DeviceClass, ip: Option<&str>) -> Result<Option<String>, String> {
    let Some(ip) = ip else { return Ok(None) };
    let ip: IpAddr = ip.trim().parse().map_err(|_| format!("invalid IP: {ip}"))?;
    let t = ssh_target(ip).unwrap_or_else(|| SshTarget::new(&ip.to_string(), class, "", "", None));
    Ok(Some(t.key()))
}

/// Stored in the OS keychain; never returned or logged.
#[tauri::command]
pub async fn set_ssh_login(input: SshLoginInput) -> Result<(), String> {
    let secret = match (
        input.password.filter(|p| !p.is_empty()),
        input.private_key.filter(|k| !k.trim().is_empty()),
    ) {
        (Some(_), Some(_)) => return Err("give a password or a private key, not both".into()),
        (Some(p), None) => SshSecret::Password(p),
        (None, Some(k)) => {
            russh::keys::decode_secret_key(&k, None).map_err(|_| {
                "not a readable unencrypted private key (OpenSSH or PEM)".to_string()
            })?;
            SshSecret::PrivateKey(k)
        }
        (None, None) => return Err("a password or private key is required".into()),
    };
    if input.username.trim().is_empty() {
        return Err("username is required".into());
    }
    let dev = dev_key(input.class, input.ip.as_deref())?;
    let kv: &dyn SecretKv = &KeyringKv;
    kv.set_login(
        input.class,
        dev.as_deref(),
        &SshLogin {
            username: input.username.trim().into(),
            secret,
        },
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_ssh_login(class: DeviceClass, ip: Option<String>) -> Result<(), String> {
    let dev = dev_key(class, ip.as_deref())?;
    let kv: &dyn SecretKv = &KeyringKv;
    kv.delete_login(class, dev.as_deref())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct SshJobRequest {
    pub ips: Vec<String>,
    #[serde(flatten)]
    pub req: SshRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct SshDoneEvent {
    pub run_id: String,
    pub results: Vec<SshOutcome>,
}

#[tauri::command]
pub async fn run_ssh_action(app: AppHandle, job: SshJobRequest) -> Result<String, String> {
    let ips = parse_ips(&job.ips)?;
    super::ssh::preflight(&job.req, ips.len())?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let rid = run_id.clone();
    tauri::async_runtime::spawn(async move {
        // App runner: SSH paths not confirmed on hardware stay dry run only.
        let runner = SshRunner {
            transport: Arc::new(RusshTransport::default()),
            kv: Arc::new(KeyringKv),
            audit: audit_sink(&app),
            opts: SshOptions::default(),
        };
        let mut results = Vec::new();
        for ip in ips {
            let o = match ssh_target(ip) {
                Some(t) => runner.run(&rid, &t, &job.req).await,
                None => SshOutcome {
                    ip: ip.to_string(),
                    action: job.req.action,
                    status: super::ssh::SshStatus::Failed,
                    msg: "miner not discovered yet; run discovery first".into(),
                    plan: Vec::new(),
                    paths_tried: Vec::new(),
                    fingerprint: None,
                    output: None,
                    backup: None,
                },
            };
            let _ = app.emit(EVENT_SSH_ROW, &o);
            results.push(o);
        }
        let _ = app.emit(
            EVENT_SSH_DONE,
            SshDoneEvent {
                run_id: rid,
                results,
            },
        );
    });
    Ok(run_id)
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct DivertJobRequest {
    pub ips: Vec<String>,
    pub op: DivertOp,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default)]
    pub confirm: Confirmation,
}

#[derive(Debug, Clone, Serialize)]
pub struct DivertDoneEvent {
    pub run_id: String,
    pub results: Vec<DivertOutcome>,
}

#[tauri::command]
pub async fn run_pool_divert(app: AppHandle, job: DivertJobRequest) -> Result<String, String> {
    let ips = parse_ips(&job.ips)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let rid = run_id.clone();
    tauri::async_runtime::spawn(async move {
        let r = resolver(Duration::from_secs(10));
        let snapshots = match app.path().app_data_dir() {
            Ok(dir) => SnapshotStore::with_file(1000, dir.join(SNAPSHOT_FILE)),
            Err(_) => SnapshotStore::in_memory(1000),
        };
        let engine = SafetyEngine::new(
            audit_sink(&app),
            Arc::new(snapshots),
            SafetyOptions::default(),
        );
        let req = DivertRequest {
            op: job.op,
            dry_run: job.dry_run,
            confirm: job.confirm.clone(),
        };
        let mut results = Vec::new();
        for ip in ips {
            let o = match r.resolve(ip, Duration::from_secs(10)).await {
                Resolved::Ready { driver, .. } => {
                    divert::run_divert(&engine, &KeyringKv, &rid, &Target::new(ip, driver), &req)
                        .await
                }
                other => DivertOutcome {
                    ip: ip.to_string(),
                    op: job.op,
                    status: super::safety::WriteStatus::Failed,
                    msg: Some(match other {
                        Resolved::Changed(m) | Resolved::Failed(m) => m,
                        Resolved::NotFound(m) => m.unwrap_or_else(|| "miner not found".into()),
                        Resolved::Ready { .. } => unreachable!(),
                    }),
                    paths_tried: Vec::new(),
                    before: None,
                    after: None,
                    snapshot_id: None,
                },
            };
            let _ = app.emit(EVENT_DIVERT_ROW, &o);
            results.push(o);
        }
        let _ = app.emit(
            EVENT_DIVERT_DONE,
            DivertDoneEvent {
                run_id: rid,
                results,
            },
        );
    });
    Ok(run_id)
}
