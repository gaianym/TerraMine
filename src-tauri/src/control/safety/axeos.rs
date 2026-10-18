//! AxeOS (Bitaxe) settings over HTTP — the one override path, used only
//! by the Bitaxe driver (asic-rs 0.4.2 exposes no Bitaxe settings writes).
//! Reads `frequency` / `coreVoltage` / fan from `/api/system/info`, writes
//! with `PATCH /api/system`. Real AxeOS applies frequency/voltage on
//! restart; not hardware-confirmed yet.

use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::{json, Value};

use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver};

pub struct AxeOsSettings {
    base: String,
    client: Client,
}

impl AxeOsSettings {
    pub fn new(base: String, client: Client) -> Self {
        Self { base, client }
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
        tuning_pct: None,
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

/// Hashrate (GH/s) and chip temperature from system info.
pub fn health_from_info(v: &Value) -> HealthSample {
    HealthSample {
        reachable: true,
        hashrate_ghs: v.get("hashRate").and_then(|h| h.as_f64()),
        temp_c: v.get("temp").and_then(|t| t.as_f64()),
    }
}

impl SettingsDriver for AxeOsSettings {
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
        if change.tuning_pct.is_some() {
            return Err("tuning_pct is not writable over AxeOS".into());
        }
        Ok(())
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move {
            let url = format!("{}/api/system/info", self.base);
            let v: Value = self.client.get(url).send().await?.error_for_status()?.json().await?;
            Ok(parse_axeos_settings(&v))
        })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let body = axeos_patch_body(s);
        Box::pin(async move {
            let url = format!("{}/api/system", self.base);
            self.client.patch(url).json(&body).send().await?.error_for_status()?;
            Ok(())
        })
    }
}
