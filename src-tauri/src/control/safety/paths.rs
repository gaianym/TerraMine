//! Write paths and the fallback chain that picks one per device.
//!
//! Each path declares which device classes it accepts *before* sending
//! anything. A path that does not accept a device is skipped with a
//! reason, and every attempt is reported so a total failure lists the
//! paths tried. Paths not yet exercised on real hardware are marked
//! `hardware_confirmed = false` and are skipped unless the caller opts in
//! (tests against the mock miner).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::{json, Value};

use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver};
use crate::control::overrides::bitaxe::looks_like_axeos;

#[derive(Debug, Clone)]
pub struct Target {
    pub ip: IpAddr,
    pub profile: DeviceProfile,
    pub http_base: String,
    pub cgminer_addr: String,
}

impl Target {
    pub fn for_ip(ip: IpAddr, profile: DeviceProfile) -> Self {
        Self { ip, profile, http_base: format!("http://{ip}"), cgminer_addr: format!("{ip}:4028") }
    }
}

pub enum Open {
    Opened(Box<dyn SettingsDriver>),
    Skip(String),
}

pub trait WritePath: Send + Sync {
    fn id(&self) -> &'static str;
    /// False until the path has been verified against real hardware.
    fn hardware_confirmed(&self) -> bool;
    /// Static check (no I/O). Err = skip reason.
    fn accepts(&self, p: &DeviceProfile, change: &Settings) -> Result<(), String>;
    /// Probe the endpoint read-only and open a driver.
    fn open<'a>(&'a self, t: &'a Target, timeout: Duration) -> BoxFuture<'a, anyhow::Result<Open>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Skipped { path: &'static str, reason: String },
    Failed { path: &'static str, error: String },
    Opened { path: &'static str },
}

impl std::fmt::Display for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skipped { path, reason } => write!(f, "{path}: skipped ({reason})"),
            Self::Failed { path, error } => write!(f, "{path}: failed ({error})"),
            Self::Opened { path } => write!(f, "{path}: opened"),
        }
    }
}

pub struct FallbackChain {
    pub paths: Vec<Arc<dyn WritePath>>,
}

impl FallbackChain {
    pub fn builtin() -> Self {
        Self { paths: vec![Arc::new(AxeOsHttpPath), Arc::new(CgminerRpcPath)] }
    }

    pub async fn open(
        &self,
        t: &Target,
        change: &Settings,
        allow_unconfirmed: bool,
        timeout: Duration,
    ) -> (Option<(&'static str, Box<dyn SettingsDriver>)>, Vec<Attempt>) {
        let mut tried = Vec::new();
        for p in &self.paths {
            let path = p.id();
            if let Err(reason) = p.accepts(&t.profile, change) {
                tried.push(Attempt::Skipped { path, reason });
                continue;
            }
            if !p.hardware_confirmed() && !allow_unconfirmed {
                tried.push(Attempt::Skipped { path, reason: "not yet confirmed on real hardware".into() });
                continue;
            }
            match tokio::time::timeout(timeout, p.open(t, timeout)).await {
                Ok(Ok(Open::Opened(d))) => {
                    tried.push(Attempt::Opened { path });
                    return (Some((path, d)), tried);
                }
                Ok(Ok(Open::Skip(reason))) => tried.push(Attempt::Skipped { path, reason }),
                Ok(Err(e)) => tried.push(Attempt::Failed { path, error: e.to_string() }),
                Err(_) => tried.push(Attempt::Failed { path, error: "timeout".into() }),
            }
        }
        (None, tried)
    }
}

// ---------------- AxeOS HTTP ----------------

/// AxeOS `PATCH /api/system`. Note: real AxeOS persists frequency/voltage
/// to NVS and applies them on restart; whether a restart is part of the
/// write is decided with the AxeOS settings PR. Not hardware-confirmed.
pub struct AxeOsHttpPath;

impl WritePath for AxeOsHttpPath {
    fn id(&self) -> &'static str {
        "axeos-http"
    }
    fn hardware_confirmed(&self) -> bool {
        false
    }
    fn accepts(&self, p: &DeviceProfile, change: &Settings) -> Result<(), String> {
        if p.class != DeviceClass::AxeOs {
            return Err(format!("device class {:?} is not AxeOS", p.class));
        }
        if change.power_limit_w.is_some() {
            return Err("power_limit_w is not writable over AxeOS".into());
        }
        Ok(())
    }
    fn open<'a>(&'a self, t: &'a Target, timeout: Duration) -> BoxFuture<'a, anyhow::Result<Open>> {
        Box::pin(async move {
            let client = Client::builder().timeout(timeout).no_proxy().build()?;
            let d = AxeOsSettings { base: t.http_base.clone(), client };
            // Read-only identity check before any write is allowed.
            match d.info().await {
                Ok(v) if looks_like_axeos(&v) => Ok(Open::Opened(Box::new(d))),
                Ok(_) => Ok(Open::Skip("endpoint did not answer as AxeOS".into())),
                Err(e) => Ok(Open::Skip(format!("AxeOS info probe failed: {e}"))),
            }
        })
    }
}

pub struct AxeOsSettings {
    base: String,
    client: Client,
}

impl AxeOsSettings {
    async fn info(&self) -> anyhow::Result<Value> {
        let url = format!("{}/api/system/info", self.base);
        Ok(self.client.get(url).send().await?.error_for_status()?.json().await?)
    }
}

fn as_u32(v: Option<&Value>) -> Option<u32> {
    v.and_then(|v| v.as_f64()).map(|f| f.round() as u32)
}

pub fn parse_axeos_settings(v: &Value) -> Settings {
    Settings {
        frequency_mhz: as_u32(v.get("frequency")),
        core_voltage_mv: as_u32(v.get("coreVoltage")),
        fan_pct: as_u32(v.get("fanspeed")),
        auto_fan: v.get("autofanspeed").and_then(|a| a.as_bool().or_else(|| a.as_u64().map(|n| n != 0))),
        power_limit_w: None,
    }
}

pub fn axeos_patch_body(s: &Settings) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(f) = s.frequency_mhz {
        m.insert("frequency".into(), json!(f));
    }
    if let Some(v) = s.core_voltage_mv {
        m.insert("coreVoltage".into(), json!(v));
    }
    if let Some(f) = s.fan_pct {
        m.insert("fanspeed".into(), json!(f));
    }
    if let Some(a) = s.auto_fan {
        m.insert("autofanspeed".into(), json!(u8::from(a)));
    }
    Value::Object(m)
}

impl SettingsDriver for AxeOsSettings {
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move { Ok(parse_axeos_settings(&self.info().await?)) })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let body = axeos_patch_body(s);
        Box::pin(async move {
            let url = format!("{}/api/system", self.base);
            self.client.patch(url).json(&body).send().await?.error_for_status()?;
            Ok(())
        })
    }
    fn health(&self) -> BoxFuture<'_, HealthSample> {
        Box::pin(async move {
            match self.info().await {
                Ok(v) => HealthSample {
                    reachable: true,
                    hashrate_ghs: v.get("hashRate").and_then(|h| h.as_f64()),
                    temp_c: v.get("temp").and_then(|t| t.as_f64()),
                },
                Err(_) => HealthSample::unreachable(),
            }
        })
    }
}

// ---------------- cgminer RPC ----------------

/// cgminer-family devices. Plain cgminer/bmminer RPC exposes no persistent
/// settings writes, so this path accepts cgminer-family devices but skips
/// every settings field with a reason; firmware-specific writes
/// (BraiinsOS, LuxOS, ...) arrive as their own paths.
pub struct CgminerRpcPath;

impl WritePath for CgminerRpcPath {
    fn id(&self) -> &'static str {
        "cgminer-rpc"
    }
    fn hardware_confirmed(&self) -> bool {
        false
    }
    fn accepts(&self, p: &DeviceProfile, change: &Settings) -> Result<(), String> {
        if !p.class.is_cgminer_family() {
            return Err(format!("device class {:?} does not speak cgminer RPC", p.class));
        }
        if !change.is_empty() {
            return Err("no settings writes over plain cgminer RPC".into());
        }
        Ok(())
    }
    fn open<'a>(&'a self, _t: &'a Target, _timeout: Duration) -> BoxFuture<'a, anyhow::Result<Open>> {
        Box::pin(async { Ok(Open::Skip("no settings writes over plain cgminer RPC".into())) })
    }
}
