//! AxeOS (Bitaxe) settings over HTTP — the one override path, used only
//! by the Bitaxe driver (asic-rs 0.8.6 exposes no Bitaxe settings writes).
//!
//! - Snapshot / read-back: `GET /api/system/info` — `frequency`,
//!   `coreVoltage`, `autofanspeed`, `temptarget` and the manual fan speed
//!   (`manualFanSpeed` since AxeOS 2.11, where `fanspeed` became the
//!   measured fan %; `fanspeed` before that).
//! - Write: `PATCH /api/system`. Before AxeOS 2.15, frequency / core
//!   voltage then need `POST /api/system/restart` to apply; the engine
//!   waits for the restart (uptime reset). On 2.15.x the firmware applies
//!   a frequency live (ESP-Miner's power-management loop picks up the NVS
//!   change), so a frequency-only change sends no restart; a change that
//!   includes core voltage still restarts (not hardware-checked live yet).
//!   `actualFrequency` is checked either way.
//! - Power: AxeOS has no power limit. A `power_limit_w` request is mapped
//!   to a frequency stand-in estimated from the reported power draw.
//!
//! Hardware-confirmed on AxeOS 2.15.x (Bitaxe Gamma, v2.15.3, Oct 17 2026,
//! `tests/hw_axeos_settings.rs`) for frequency and the fan settings (manual
//! %, auto fan, target temperature) only. Core voltage and the power
//! stand-in, and every other AxeOS version, stay dry-run only.

use futures::future::BoxFuture;
use reqwest::Client;
use semver::Version;
use serde_json::{json, Value};

use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver};

/// Frequency stand-ins are rounded down to this step.
pub const FREQ_STEP_MHZ: u32 = 5;

/// AxeOS `major.minor` lines where frequency and fan writes are
/// hardware-confirmed, and where a frequency applies without a restart.
pub const HW_CONFIRMED_LINES: [(u64, u64); 1] = [(2, 15)];

/// Is `v` on a hardware-confirmed AxeOS line?
pub fn confirmed_line(v: Option<&Version>) -> bool {
    v.is_some_and(|v| HW_CONFIRMED_LINES.contains(&(v.major, v.minor)))
}

/// The change only touches fields confirmed on hardware (frequency, fan).
pub fn confirmed_fields(change: &Settings) -> bool {
    change.core_voltage_mv.is_none()
        && change.power_limit_w.is_none()
        && change.tuning_pct.is_none()
        && change.temp_hot_c.is_none()
        && change.temp_danger_c.is_none()
        && (change.frequency_mhz.is_some() || change.touches_fan())
}

pub struct AxeOsSettings {
    base: String,
    client: Client,
    version: Option<Version>,
}

impl AxeOsSettings {
    pub fn new(base: String, client: Client) -> Self {
        Self { base, client, version: None }
    }

    /// Firmware version, which decides restart-to-apply and confirmation.
    pub fn with_version(mut self, version: Option<Version>) -> Self {
        self.version = version;
        self
    }

    /// A restart is needed to apply `change` on this firmware.
    pub fn restart_needed(&self, change: &Settings) -> bool {
        if confirmed_line(self.version.as_ref()) {
            change.core_voltage_mv.is_some()
        } else {
            needs_restart(change)
        }
    }

    async fn info(&self) -> anyhow::Result<Value> {
        let url = format!("{}/api/system/info", self.base);
        Ok(self.client.get(url).send().await?.error_for_status()?.json().await?)
    }
}

fn as_u32(v: Option<&Value>) -> Option<u32> {
    v.and_then(|v| v.as_f64()).map(|f| f.round() as u32)
}

/// The manual fan speed key: `manualFanSpeed` on AxeOS ≥ 2.11, else `fanspeed`.
pub fn fan_key(info: &Value) -> &'static str {
    if info.get("manualFanSpeed").is_some() {
        "manualFanSpeed"
    } else {
        "fanspeed"
    }
}

pub fn parse_axeos_settings(v: &Value) -> Settings {
    Settings {
        frequency_mhz: as_u32(v.get("frequency")),
        core_voltage_mv: as_u32(v.get("coreVoltage")),
        fan_pct: as_u32(v.get(fan_key(v))),
        temp_target_c: as_u32(v.get("temptarget")),
        auto_fan: v.get("autofanspeed").and_then(|a| a.as_bool().or_else(|| a.as_u64().map(|n| n != 0))),
        ..Default::default()
    }
}

pub fn axeos_patch_body(s: &Settings, fan_key: &str) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(f) = s.frequency_mhz {
        m.insert("frequency".into(), json!(f));
    }
    if let Some(v) = s.core_voltage_mv {
        m.insert("coreVoltage".into(), json!(v));
    }
    if let Some(f) = s.fan_pct {
        m.insert(fan_key.into(), json!(f));
    }
    if let Some(t) = s.temp_target_c {
        m.insert("temptarget".into(), json!(t));
    }
    if let Some(a) = s.auto_fan {
        m.insert("autofanspeed".into(), json!(u8::from(a)));
    }
    Value::Object(m)
}

/// Frequency / core voltage are applied by a restart (before AxeOS 2.15;
/// see [`AxeOsSettings::restart_needed`]).
pub fn needs_restart(s: &Settings) -> bool {
    s.frequency_mhz.is_some() || s.core_voltage_mv.is_some()
}

/// Map a power target to a frequency (power scales ~linearly with
/// frequency at a fixed core voltage). Rounded down to [`FREQ_STEP_MHZ`],
/// so the estimate errs low. The engine still applies the per-model
/// frequency limits and the frequency-raise hold to the result.
pub fn power_stand_in(change: &Settings, info: &Value) -> Result<(Settings, String), String> {
    let Some(w) = change.power_limit_w else {
        return Ok((change.clone(), String::new()));
    };
    let cur_w = info.get("power").and_then(Value::as_f64).filter(|p| *p > 0.5);
    let cur_f = info.get("frequency").and_then(Value::as_f64).filter(|f| *f > 0.0);
    let (Some(cur_w), Some(cur_f)) = (cur_w, cur_f) else {
        return Err("AxeOS has no power limit, and power draw / frequency are not reported (miner paused?), so no frequency stand-in can be estimated".into());
    };
    let est = cur_f * f64::from(w) / cur_w;
    let f = ((est / f64::from(FREQ_STEP_MHZ)).floor() as u32) * FREQ_STEP_MHZ;
    let mut out = change.clone();
    out.power_limit_w = None;
    out.frequency_mhz = Some(f);
    let volt = if change.core_voltage_mv.is_some() { "" } else { ", core voltage unchanged" };
    let note = format!(
        "power_limit_w {w} W: AxeOS has no power limit; stand-in frequency {cur_f:.0} → {f} MHz (estimated from {cur_w:.1} W at {cur_f:.0} MHz{volt})"
    );
    Ok((out, note))
}

/// Is `want.frequency_mhz` actually running (`actualFrequency`)? Tolerance
/// is the larger of 2 MHz and 1%. Firmware that does not report
/// `actualFrequency` passes (configured value only).
pub fn frequency_applied(info: &Value, want: &Settings) -> Result<(), String> {
    let (Some(f), Some(actual)) = (want.frequency_mhz, info.get("actualFrequency").and_then(Value::as_f64)) else {
        return Ok(());
    };
    let tol = (f64::from(f) * 0.01).max(2.0);
    if (actual - f64::from(f)).abs() <= tol {
        Ok(())
    } else {
        Err(format!("actualFrequency {actual:.0} MHz, want {f} MHz (not applied yet)"))
    }
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
    fn confirmed_for(&self, change: &Settings) -> bool {
        confirmed_line(self.version.as_ref()) && confirmed_fields(change)
    }
    fn accepts(&self, p: &DeviceProfile, change: &Settings) -> Result<(), String> {
        if p.class != DeviceClass::AxeOs {
            return Err(format!("device class {:?} is not AxeOS", p.class));
        }
        if change.temp_hot_c.is_some() || change.temp_danger_c.is_some() {
            return Err("AxeOS has no thermal threshold setting (overheat protection is fixed in firmware)".into());
        }
        if change.tuning_pct.is_some() {
            return Err("tuning_pct has no AxeOS equivalent; use a power target (mapped to frequency) or a frequency".into());
        }
        if change.power_limit_w.is_some() && change.frequency_mhz.is_some() {
            return Err("power_limit_w is mapped to a frequency on AxeOS; send either power_limit_w or frequency_mhz".into());
        }
        Ok(())
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move { Ok(parse_axeos_settings(&self.info().await?)) })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let s = s.clone();
        Box::pin(async move {
            let key = if s.fan_pct.is_some() { fan_key(&self.info().await?) } else { "fanspeed" };
            let url = format!("{}/api/system", self.base);
            self.client.patch(url).json(&axeos_patch_body(&s, key)).send().await?.error_for_status()?;
            Ok(())
        })
    }
    fn translate<'a>(&'a self, change: &'a Settings) -> BoxFuture<'a, Result<(Settings, Option<String>), String>> {
        Box::pin(async move {
            if change.power_limit_w.is_none() {
                return Ok((change.clone(), None));
            }
            let info = self.info().await.map_err(|e| format!("cannot read power draw for the AxeOS power stand-in: {e}"))?;
            power_stand_in(change, &info).map(|(s, n)| (s, Some(n)))
        })
    }
    fn restart_to_apply(&self, change: &Settings) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        if !self.restart_needed(change) {
            return None;
        }
        Some(Box::pin(async move {
            let url = format!("{}/api/system/restart", self.base);
            self.client.post(url).send().await?.error_for_status()?;
            Ok(())
        }))
    }
    fn uptime_s(&self) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
        Box::pin(async move { Ok(self.info().await?.get("uptimeSeconds").and_then(Value::as_u64)) })
    }
    fn check_applied<'a>(&'a self, want: &'a Settings) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if want.frequency_mhz.is_none() {
                return Ok(());
            }
            let info = self.info().await.map_err(|e| e.to_string())?;
            frequency_applied(&info, want)
        })
    }
}
