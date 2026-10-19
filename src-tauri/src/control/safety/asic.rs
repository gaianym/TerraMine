//! asic-rs (0.8.6) backed settings and health. Writes go through the
//! typed asic-rs setters and are gated by its `supports_*()` flags;
//! snapshot / read-back use the matching asic-rs getters.
//!
//! Power limit: only `set_power_limit` (`supports_set_power_limit()`),
//! read back from the reported tuning target. `set_tuning_config(Power)`
//! is NOT used as a stand-in: stock Antminer, VNish and Sealminer report
//! `supports_tuning_config() = true` but reject or remap a power target
//! (tuning targets / presets are plan PR D).
//! Tuning percent: `set_tuning_percent` / `get_tuning_percent`
//! (`supports_set_tuning_percent()`, VNish 1.3 in asic-rs 0.8.6).
//! Fan: `set_fan_config` / `get_fan_config` (`supports_fan_config()`: stock
//! Antminer, ePIC, Proto in 0.8.6). Manual % or auto with a target
//! temperature; stock Antminer auto only toggles `bitmain-fan-ctrl` (no
//! target), so a target there is refused. Auto writes keep the firmware's
//! current idle speed.
//! Temperature thresholds: `get_temperature_config` is read only
//! (`supports_temperature_config()`: VNish 1.2 / 1.3); no 0.8.6 firmware
//! implements `set_temperature_config`, so threshold writes are refused.
//! Pools (plan PR F): `set_pools_config` / `get_pools_config`
//! (`supports_pools_config()`), read back by URL and worker; after the
//! write the runtime pool status (`get_pools`) must show one of the new
//! pools alive. A password the firmware does not report is restored as
//! `x` on rollback (the asic-rs default).
//! Timezone: `set_timezone_config` / `get_timezone_config`
//! (`supports_timezone_config()`: Braiins 25.03+, ePIC, VNish); the zone
//! must be one the miner lists as available.

use asic_rs_core::config::fan::FanConfig;
use asic_rs_core::config::pools::{PoolConfig, PoolGroupConfig};
use asic_rs_core::config::timezone::TimezoneConfig;
use asic_rs_core::data::pool::{PoolGroupData, PoolURL};
use asic_rs_core::config::temperature::TemperatureConfig;
use asic_rs_core::data::hashrate::HashRateUnit;
use asic_rs_core::data::miner::{MinerData, TuningTarget};
use std::sync::Arc;

use futures::future::BoxFuture;
use measurements::Power;

use super::pools::{normalize_url, parse_timezone, PoolGroupSpec, PoolSpec, Secret};
use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver};
use crate::control::driver::SharedMiner;

/// Health from asic-rs telemetry: hashrate in GH/s and the hottest
/// reported board/chip temperature.
pub fn health_from_miner_data(d: &MinerData) -> HealthSample {
    let hashrate_ghs = d.hashrate.clone().map(|h| h.as_unit(HashRateUnit::GigaHash).value);
    let temps = d
        .hashboards
        .iter()
        .flat_map(|b| [b.board_temperature, b.inlet_chip_temperature, b.outlet_chip_temperature])
        .chain([d.average_temperature])
        .flatten()
        .map(|t| t.as_celsius());
    let temp_c = temps.fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
    let reachable = d.hashrate.is_some() || d.uptime.is_some() || !d.hashboards.is_empty();
    HealthSample { reachable, hashrate_ghs, temp_c }
}

/// asic-rs support flags for the settings it can write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeSettingsFlags {
    /// `supports_pools_config()`.
    pub pools: bool,
    /// `supports_timezone_config()`.
    pub timezone: bool,
    pub power_limit: bool,
    pub tuning_percent: bool,
    pub fan: bool,
    /// Auto fan honours a target temperature (false on stock Antminer).
    pub auto_target: bool,
    /// Thermal thresholds readable (`supports_temperature_config()`).
    pub temperature_config: bool,
}

/// Settings fields asic-rs can write, given its support flags.
pub fn asic_accepts(change: &Settings, f: NativeSettingsFlags) -> Result<(), String> {
    let mut why = Vec::new();
    if change.touches_pools() && !f.pools {
        why.push("asic-rs: pools not supported on this firmware (supports_pools_config() = false)".to_string());
    }
    if let Some(tz) = &change.timezone {
        if !f.timezone {
            why.push("asic-rs: timezone not supported on this firmware (supports_timezone_config() = false)".into());
        } else if let Err(e) = parse_timezone(tz) {
            why.push(e);
        }
    }
    if change.frequency_mhz.is_some() || change.core_voltage_mv.is_some() {
        why.push("asic-rs has no manual frequency/voltage target".to_string());
    }
    if change.power_limit_w.is_some() && !f.power_limit {
        why.push("asic-rs: power limit not supported on this firmware (supports_set_power_limit() = false)".into());
    }
    if change.tuning_pct.is_some() && !f.tuning_percent {
        why.push("asic-rs: tuning percent not supported on this firmware (supports_set_tuning_percent() = false)".into());
    }
    if change.touches_fan() {
        if !f.fan {
            why.push("asic-rs: fan config not supported on this firmware (supports_fan_config() = false)".into());
        } else if let Some(e) = fan_combination(change, f.auto_target) {
            why.push(e);
        }
    }
    if change.temp_hot_c.is_some() || change.temp_danger_c.is_some() {
        why.push(format!(
            "asic-rs 0.8.6: supports_temperature_config() = {}; no firmware implements set_temperature_config (thresholds are read only)",
            f.temperature_config
        ));
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(why.join("; "))
    }
}

/// Fan fields that do not form one asic-rs `FanConfig`.
fn fan_combination(c: &Settings, auto_target: bool) -> Option<String> {
    let auto = c.auto_fan;
    if c.temp_target_c.is_some() && !auto_target {
        return Some("stock Antminer auto fan has no target temperature (asic-rs writes bitmain-fan-ctrl only)".into());
    }
    match auto {
        Some(true) if c.fan_pct.is_some() => Some("send either auto fan or a manual fan_pct, not both".into()),
        Some(false) if c.fan_pct.is_none() => Some("manual fan needs fan_pct".into()),
        Some(true) if auto_target && c.temp_target_c.is_none() => {
            Some("auto fan needs a target temperature (temp_target_c) on this firmware".into())
        }
        Some(false) if c.temp_target_c.is_some() => Some("a target temperature only applies in auto fan mode".into()),
        None if c.temp_target_c.is_some() => Some("temp_target_c needs auto_fan: true".into()),
        _ => None,
    }
}

/// The `FanConfig` for the fan fields of `s` (manual wins when only a
/// speed is given). `idle` keeps the firmware's current auto idle speed.
pub fn fan_config_for(s: &Settings, idle: Option<u64>) -> Option<FanConfig> {
    match (s.auto_fan, s.fan_pct) {
        (Some(true), _) => Some(FanConfig::Auto { target_temp: s.temp_target_c.unwrap_or(0) as f64, idle_speed: idle }),
        (_, Some(p)) => Some(FanConfig::Manual { fan_speed: p as u64 }),
        _ => None,
    }
}

pub fn settings_from_configs(power_target: Option<&TuningTarget>, fan: Option<&FanConfig>) -> Settings {
    Settings {
        power_limit_w: power_target.and_then(|t| match t {
            TuningTarget::Power(p) => Some(p.as_watts().round() as u32),
            _ => None,
        }),
        fan_pct: fan.and_then(|f| match f {
            FanConfig::Manual { fan_speed } => Some(*fan_speed as u32),
            FanConfig::Auto { .. } => None,
        }),
        auto_fan: fan.map(|f| matches!(f, FanConfig::Auto { .. })),
        temp_target_c: fan.and_then(|f| match f {
            FanConfig::Auto { target_temp, .. } if *target_temp > 0.0 => Some(target_temp.round() as u32),
            _ => None,
        }),
        ..Default::default()
    }
}

pub fn pools_from_config(groups: &[PoolGroupConfig]) -> Vec<PoolGroupSpec> {
    groups
        .iter()
        .map(|g| PoolGroupSpec {
            name: g.name.clone(),
            quota: g.quota,
            pools: g
                .pools
                .iter()
                .map(|p| PoolSpec { url: p.url.to_string(), user: p.username.clone(), password: Secret::Value(p.password.clone()) })
                .collect(),
        })
        .collect()
}

pub fn pools_to_config(groups: &[PoolGroupSpec]) -> Vec<PoolGroupConfig> {
    groups
        .iter()
        .map(|g| PoolGroupConfig {
            name: if g.name.is_empty() { "default".into() } else { g.name.clone() },
            quota: g.quota.max(1),
            pools: g
                .pools
                .iter()
                .map(|p| PoolConfig {
                    url: PoolURL::from(p.url.trim().to_string()),
                    username: p.user.trim().to_string(),
                    password: p.password.value().unwrap_or("x").to_string(),
                })
                .collect(),
        })
        .collect()
}

/// After a pool write: one of the wanted pools must be reported alive (or
/// active). Firmware that reports no status for them passes.
pub fn pools_alive(status: &[PoolGroupData], want: &[PoolGroupSpec]) -> Result<(), String> {
    let wanted: Vec<String> = super::pools::flatten(want).into_iter().map(|(u, _)| u).collect();
    let seen: Vec<(String, Option<bool>, Option<bool>)> = status
        .iter()
        .flat_map(|g| g.pools.iter())
        .filter_map(|p| Some((normalize_url(&p.url.as_ref()?.to_string()), p.alive, p.active)))
        .filter(|(u, _, _)| wanted.contains(u))
        .collect();
    if seen.is_empty() || seen.iter().all(|(_, a, b)| a.is_none() && b.is_none()) {
        return Ok(());
    }
    if seen.iter().any(|(_, a, b)| *a == Some(true) || *b == Some(true)) {
        Ok(())
    } else {
        Err(format!("none of the new pools is alive yet ({})", wanted.join(", ")))
    }
}

/// The asic-rs calls the settings backend makes. One implementation wraps
/// a real asic-rs miner; mock tests provide per-firmware stand-ins (asic-rs
/// hardcodes real ports, so it cannot reach a local mock).
pub trait NativeTuning: Send + Sync {
    fn flags(&self) -> NativeSettingsFlags;
    fn pools_config(&self) -> BoxFuture<'_, Option<Vec<PoolGroupConfig>>> {
        Box::pin(async { None })
    }
    fn set_pools_config(&self, _c: Vec<PoolGroupConfig>) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("Setting pools is not supported on this platform") })
    }
    /// Runtime pool status (`get_pools`).
    fn pool_status(&self) -> BoxFuture<'_, Vec<PoolGroupData>> {
        Box::pin(async { Vec::new() })
    }
    fn timezone_config(&self) -> BoxFuture<'_, Option<TimezoneConfig>> {
        Box::pin(async { None })
    }
    fn set_timezone_config(&self, _c: TimezoneConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("Setting timezone config is not supported on this platform") })
    }
    /// Power limit currently in force (W), when the firmware reports one.
    fn power_target(&self) -> BoxFuture<'_, Option<u32>>;
    fn set_power_limit(&self, watts: u32) -> BoxFuture<'_, anyhow::Result<bool>>;
    fn tuning_percent(&self) -> BoxFuture<'_, Option<u32>>;
    fn set_tuning_percent(&self, pct: u8) -> BoxFuture<'_, anyhow::Result<bool>>;
    fn fan(&self) -> BoxFuture<'_, Option<FanConfig>>;
    fn set_fan(&self, f: FanConfig) -> BoxFuture<'_, anyhow::Result<bool>>;
    /// Thermal thresholds, when `flags().temperature_config`.
    fn temperature_config(&self) -> BoxFuture<'_, Option<TemperatureConfig>> {
        Box::pin(async { None })
    }
}

/// [`NativeTuning`] over a real asic-rs miner.
pub struct AsicRsMiner(pub SharedMiner);

impl NativeTuning for AsicRsMiner {
    fn flags(&self) -> NativeSettingsFlags {
        NativeSettingsFlags {
            pools: self.0.supports_pools_config(),
            timezone: self.0.supports_timezone_config(),
            power_limit: self.0.supports_set_power_limit(),
            tuning_percent: self.0.supports_set_tuning_percent(),
            fan: self.0.supports_fan_config(),
            // Refined from the device profile in `AsicRsSettings::accepts`.
            auto_target: self.0.supports_fan_config(),
            temperature_config: self.0.supports_temperature_config(),
        }
    }
    fn temperature_config(&self) -> BoxFuture<'_, Option<TemperatureConfig>> {
        Box::pin(async move { self.0.get_temperature_config().await.ok() })
    }
    fn pools_config(&self) -> BoxFuture<'_, Option<Vec<PoolGroupConfig>>> {
        Box::pin(async move { self.0.get_pools_config().await.ok() })
    }
    fn set_pools_config(&self, c: Vec<PoolGroupConfig>) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_pools_config(c).await })
    }
    fn pool_status(&self) -> BoxFuture<'_, Vec<PoolGroupData>> {
        Box::pin(async move { self.0.get_pools().await })
    }
    fn timezone_config(&self) -> BoxFuture<'_, Option<TimezoneConfig>> {
        Box::pin(async move { self.0.get_timezone_config().await.ok() })
    }
    fn set_timezone_config(&self, c: TimezoneConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_timezone_config(c).await })
    }
    fn power_target(&self) -> BoxFuture<'_, Option<u32>> {
        Box::pin(async move { settings_from_configs(self.0.get_tuning_target().await.as_ref(), None).power_limit_w })
    }
    fn set_power_limit(&self, watts: u32) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_power_limit(Power::from_watts(watts as f64)).await })
    }
    fn tuning_percent(&self) -> BoxFuture<'_, Option<u32>> {
        Box::pin(async move { self.0.get_tuning_percent().await.map(u32::from) })
    }
    fn set_tuning_percent(&self, pct: u8) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_tuning_percent(pct).await })
    }
    fn fan(&self) -> BoxFuture<'_, Option<FanConfig>> {
        Box::pin(async move { self.0.get_fan_config().await.ok() })
    }
    fn set_fan(&self, f: FanConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_fan_config(f).await })
    }
}

pub struct AsicRsSettings {
    native: Arc<dyn NativeTuning>,
    /// Last auto-fan idle speed seen (snapshot read), so a rollback from
    /// manual back to auto restores it instead of the asic-rs default.
    idle_speed: std::sync::Mutex<Option<u64>>,
}

impl AsicRsSettings {
    pub fn new(miner: SharedMiner) -> Self {
        Self::from_native(Arc::new(AsicRsMiner(miner)))
    }
    pub fn from_native(native: Arc<dyn NativeTuning>) -> Self {
        Self { native, idle_speed: std::sync::Mutex::new(None) }
    }
}

impl SettingsDriver for AsicRsSettings {
    fn id(&self) -> &'static str {
        "asic-rs"
    }
    fn hardware_confirmed(&self) -> bool {
        false
    }
    fn accepts(&self, p: &DeviceProfile, change: &Settings) -> Result<(), String> {
        let f = self.native.flags();
        let auto_target = f.auto_target && p.class != DeviceClass::AntminerStock;
        asic_accepts(change, NativeSettingsFlags { auto_target, ..f })
    }
    /// A timezone must be one the miner lists (when it lists any).
    fn translate<'a>(&'a self, change: &'a Settings) -> BoxFuture<'a, Result<(Settings, Option<String>), String>> {
        Box::pin(async move {
            if let Some(tz) = &change.timezone {
                let want = parse_timezone(tz)?;
                if let Some(c) = self.native.timezone_config().await {
                    if !c.available.is_empty() && !c.available.contains(&want) {
                        return Err(format!("timezone {tz} is not one this miner accepts"));
                    }
                }
            }
            Ok((change.clone(), None))
        })
    }
    fn check_applied<'a>(&'a self, want: &'a Settings) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match &want.pools {
                Some(g) => pools_alive(&self.native.pool_status().await, g),
                None => Ok(()),
            }
        })
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move {
            let f = self.native.flags();
            let fan = if f.fan { self.native.fan().await } else { None };
            if let Some(FanConfig::Auto { idle_speed: Some(i), .. }) = &fan {
                *self.idle_speed.lock().unwrap() = Some(*i);
            }
            let mut s = settings_from_configs(None, fan.as_ref());
            if f.pools {
                s.pools = self.native.pools_config().await.map(|g| pools_from_config(&g));
            }
            if f.timezone {
                s.timezone = self.native.timezone_config().await.and_then(|t| t.timezone).map(|t| t.name().to_string());
            }
            if f.power_limit {
                s.power_limit_w = self.native.power_target().await;
            }
            if f.tuning_percent {
                s.tuning_pct = self.native.tuning_percent().await;
            }
            if f.temperature_config {
                if let Some(t) = self.native.temperature_config().await {
                    s.temp_hot_c = t.hot.map(|v| v.round() as u32);
                    s.temp_danger_c = t.danger.map(|v| v.round() as u32);
                }
            }
            Ok(s)
        })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let s = s.clone();
        Box::pin(async move {
            if let Some(g) = &s.pools {
                anyhow::ensure!(self.native.set_pools_config(pools_to_config(g)).await?, "miner refused pool config");
            }
            if let Some(tz) = &s.timezone {
                let tz = parse_timezone(tz).map_err(|e| anyhow::anyhow!(e))?;
                let c = TimezoneConfig { timezone: Some(tz), available: Vec::new() };
                anyhow::ensure!(self.native.set_timezone_config(c).await?, "miner refused timezone");
            }
            if let Some(w) = s.power_limit_w {
                anyhow::ensure!(self.native.set_power_limit(w).await?, "miner refused power limit");
            }
            if let Some(p) = s.tuning_pct {
                let p = u8::try_from(p).map_err(|_| anyhow::anyhow!("tuning_pct {p} out of range"))?;
                anyhow::ensure!(self.native.set_tuning_percent(p).await?, "miner refused tuning percent");
            }
            if s.touches_fan() {
                let idle = match self.native.fan().await {
                    Some(FanConfig::Auto { idle_speed: Some(i), .. }) => Some(i),
                    _ => *self.idle_speed.lock().unwrap(),
                };
                let cfg = fan_config_for(&s, idle).ok_or_else(|| anyhow::anyhow!("fan change has no mode"))?;
                anyhow::ensure!(self.native.set_fan(cfg).await?, "miner refused fan config");
            }
            Ok(())
        })
    }
}
