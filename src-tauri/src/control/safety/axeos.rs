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
//! - Pools (plan PR F): AxeOS has exactly two slots, primary and
//!   fallback: `stratumURL` (host) / `stratumPort` / `stratumUser` /
//!   `stratumPassword` and the `fallbackStratum*` keys, written by the same
//!   `PATCH /api/system` and applied by a restart. Read back from
//!   `/api/system/info` (passwords are not reported, so a password is only
//!   sent when the request has one, and a rollback cannot restore an old
//!   password). After the restart `isUsingFallbackStratum` must be false,
//!   or the change is rolled back. stratum+tcp only. No timezone setting.
//! - Tuning targets (plan PR D) reuse the same frequency stand-in:
//!   a hashrate target scales frequency by target / reported `hashRate`;
//!   mode Normal is the chip's ESP-Miner default frequency and Low 10%
//!   under it. High (overclock), presets and scaling have no AxeOS
//!   equivalent and are refused with the reason.
//!
//! Hardware-confirmed on AxeOS 2.15.x (Bitaxe Gamma, v2.15.3, Oct 17 2026,
//! `tests/hw_axeos_settings.rs`) for frequency and the fan settings (manual
//! %, auto fan, target temperature) only. Core voltage and the power
//! stand-in, and every other AxeOS version, stay dry-run only.

use futures::future::BoxFuture;
use reqwest::Client;
use semver::Version;
use serde_json::{json, Value};

use super::pools::{PoolGroupSpec, PoolSpec, Secret};
use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver, TuningMode};

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
    !change.touches_pools()
        && change.timezone.is_none()
        && change.core_voltage_mv.is_none()
        && change.power_limit_w.is_none()
        && change.tuning_pct.is_none()
        && change.temp_hot_c.is_none()
        && change.temp_danger_c.is_none()
        && !change.touches_target_or_scaling()
        && (change.frequency_mhz.is_some() || change.touches_fan())
}

/// ESP-Miner default frequency (MHz) per ASIC: the "Normal" mode stand-in.
pub const STOCK_FREQUENCY_MHZ: [(&str, u32); 4] = [("BM1370", 525), ("BM1368", 490), ("BM1366", 485), ("BM1397", 425)];

/// "Low" mode stand-in: this fraction of the stock frequency.
pub const LOW_MODE_FRACTION: f64 = 0.9;

pub fn stock_frequency(chip: &str) -> Option<u32> {
    STOCK_FREQUENCY_MHZ.iter().find(|(c, _)| c.eq_ignore_ascii_case(chip.trim())).map(|x| x.1)
}

fn floor_step(f: f64) -> u32 {
    ((f / f64::from(FREQ_STEP_MHZ)).floor() as u32) * FREQ_STEP_MHZ
}

/// Map a hashrate target (GH/s) to a frequency: hashrate scales linearly
/// with frequency. Same estimate shape as [`power_stand_in`].
pub fn hashrate_stand_in(change: &Settings, info: &Value) -> Result<(Settings, String), String> {
    let Some(h) = change.tuning_hashrate_ghs else {
        return Ok((change.clone(), String::new()));
    };
    let cur_h = info.get("hashRate").and_then(Value::as_f64).filter(|h| *h > 1.0);
    let cur_f = info.get("frequency").and_then(Value::as_f64).filter(|f| *f > 0.0);
    let (Some(cur_h), Some(cur_f)) = (cur_h, cur_f) else {
        return Err("AxeOS has no hashrate target, and hashrate / frequency are not reported (miner paused?), so no frequency stand-in can be estimated".into());
    };
    let f = floor_step(cur_f * f64::from(h) / cur_h);
    let mut out = change.clone();
    out.tuning_hashrate_ghs = None;
    out.frequency_mhz = Some(f);
    let note = format!(
        "tuning_hashrate_ghs {h} GH/s: AxeOS has no hashrate target; stand-in frequency {cur_f:.0} → {f} MHz (estimated from {cur_h:.0} GH/s at {cur_f:.0} MHz, core voltage unchanged)"
    );
    Ok((out, note))
}

/// Map a mining mode to a frequency: Normal = the chip's ESP-Miner
/// default, Low = [`LOW_MODE_FRACTION`] of it. High is refused.
pub fn mode_stand_in(change: &Settings, info: &Value) -> Result<(Settings, String), String> {
    let Some(m) = change.tuning_mode else {
        return Ok((change.clone(), String::new()));
    };
    let chip = info.get("ASICModel").and_then(Value::as_str).unwrap_or("");
    let Some(stock) = stock_frequency(chip) else {
        return Err(format!("AxeOS has no mining modes, and no stock frequency is known for ASIC {chip:?}, so no frequency stand-in"));
    };
    let f = match m {
        TuningMode::Normal => stock,
        TuningMode::Low => floor_step(f64::from(stock) * LOW_MODE_FRACTION),
        TuningMode::High => return Err("AxeOS has no High mode; an overclock needs an explicit frequency and allow_frequency_raise".into()),
    };
    let mut out = change.clone();
    out.tuning_mode = None;
    out.frequency_mhz = Some(f);
    let note = format!("tuning_mode {}: AxeOS has no mining modes; stand-in frequency {f} MHz ({chip} stock {stock} MHz, core voltage unchanged)", m.as_str());
    Ok((out, note))
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
        if change.pools.is_some() {
            return true;
        }
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

/// AxeOS primary + fallback pool as one group (empty slots left out).
pub fn parse_axeos_pools(v: &Value) -> Option<Vec<PoolGroupSpec>> {
    let slot = |pre: &str| {
        let host = v.get(format!("{pre}URL"))?.as_str()?.trim().to_string();
        if host.is_empty() {
            return None;
        }
        let port = v.get(format!("{pre}Port")).and_then(Value::as_u64).unwrap_or(0);
        let user = v.get(format!("{pre}User")).and_then(Value::as_str).unwrap_or("").to_string();
        let url = if host.contains("://") { host } else { format!("stratum+tcp://{host}:{port}") };
        Some(PoolSpec { url, user, password: Secret::Unknown })
    };
    v.get("stratumURL")?;
    let pools: Vec<PoolSpec> = ["stratum", "fallbackStratum"].iter().filter_map(|p| slot(p)).collect();
    Some(vec![PoolGroupSpec { name: "default".into(), quota: 1, pools }])
}

/// PATCH keys for the AxeOS pool slots.
fn pool_patch(m: &mut serde_json::Map<String, Value>, groups: &[PoolGroupSpec]) {
    let pools: Vec<&PoolSpec> = groups.iter().flat_map(|g| g.pools.iter()).collect();
    for (pre, p) in ["stratum", "fallbackStratum"].iter().zip(pools) {
        let u = asic_rs_core::data::pool::PoolURL::from(p.url.trim().to_string());
        m.insert(format!("{pre}URL"), json!(u.host));
        m.insert(format!("{pre}Port"), json!(u.port));
        m.insert(format!("{pre}User"), json!(p.user.trim()));
        if let Some(pw) = p.password.value() {
            m.insert(format!("{pre}Password"), json!(pw));
        }
    }
}

/// AxeOS pool constraints on a resolved list.
pub fn axeos_pools_ok(groups: &[PoolGroupSpec]) -> Result<(), String> {
    let pools: Vec<&PoolSpec> = groups.iter().flat_map(|g| g.pools.iter()).collect();
    if pools.len() != 2 {
        return Err(format!(
            "AxeOS always has a primary and a fallback pool; the result has {} (send two pools, or replace one)",
            pools.len()
        ));
    }
    for p in pools {
        let u = asic_rs_core::data::pool::PoolURL::from(p.url.trim().to_string());
        if u.scheme.to_string() != "stratum+tcp" {
            return Err(format!("AxeOS pools here are stratum+tcp only ({} is {})", p.url, u.scheme));
        }
    }
    Ok(())
}

pub fn parse_axeos_settings(v: &Value) -> Settings {
    Settings {
        pools: parse_axeos_pools(v),
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
    if let Some(g) = &s.pools {
        pool_patch(&mut m, g);
    }
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
    let f = floor_step(cur_f * f64::from(w) / cur_w);
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
        if change.timezone.is_some() {
            return Err("AxeOS has no timezone setting".into());
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
        if change.tuning_preset.is_some() {
            return Err("AxeOS has no tuning presets".into());
        }
        if change.touches_scaling() {
            return Err("AxeOS has no scaling (throttle step / minimum) setting".into());
        }
        if change.tuning_algorithm.is_some() {
            return Err("AxeOS has no tuning algorithm setting".into());
        }
        if change.tuning_mode == Some(TuningMode::High) {
            return Err("AxeOS has no High mode; an overclock needs an explicit frequency and allow_frequency_raise".into());
        }
        if (change.tuning_mode.is_some() || change.tuning_hashrate_ghs.is_some())
            && (change.frequency_mhz.is_some() || change.target_kinds() > 1)
        {
            return Err("a tuning target is mapped to a frequency on AxeOS; send one of power, hashrate, mode or frequency".into());
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
            if let Some(g) = &change.pools {
                axeos_pools_ok(g)?;
                if g.iter().flat_map(|g| g.pools.iter()).any(|p| p.password.value().is_some()) {
                    let note = "pool password sent; AxeOS does not report passwords, so a rollback cannot restore the previous one".to_string();
                    return Ok((change.clone(), Some(note)));
                }
            }
            if change.power_limit_w.is_none() && change.tuning_hashrate_ghs.is_none() && change.tuning_mode.is_none() {
                return Ok((change.clone(), None));
            }
            let info = self.info().await.map_err(|e| format!("cannot read system info for the AxeOS frequency stand-in: {e}"))?;
            let (s, n) = if change.power_limit_w.is_some() {
                power_stand_in(change, &info)?
            } else if change.tuning_hashrate_ghs.is_some() {
                hashrate_stand_in(change, &info)?
            } else {
                mode_stand_in(change, &info)?
            };
            Ok((s, Some(n)))
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
            if want.frequency_mhz.is_none() && want.pools.is_none() {
                return Ok(());
            }
            let info = self.info().await.map_err(|e| e.to_string())?;
            if want.pools.is_some() && info.get("isUsingFallbackStratum").and_then(Value::as_bool) == Some(true) {
                return Err("miner is on its fallback pool: the new primary pool is not reachable".into());
            }
            frequency_applied(&info, want)
        })
    }
}
