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

use asic_rs_core::config::fan::FanConfig;
use asic_rs_core::data::hashrate::HashRateUnit;
use asic_rs_core::data::miner::{MinerData, TuningTarget};
use std::sync::Arc;

use futures::future::BoxFuture;
use measurements::Power;

use super::profile::DeviceProfile;
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
    pub power_limit: bool,
    pub tuning_percent: bool,
    pub fan: bool,
}

/// Settings fields asic-rs can write, given its support flags.
pub fn asic_accepts(change: &Settings, f: NativeSettingsFlags) -> Result<(), String> {
    let mut why = Vec::new();
    if change.frequency_mhz.is_some() || change.core_voltage_mv.is_some() {
        why.push("asic-rs has no manual frequency/voltage target".to_string());
    }
    if change.power_limit_w.is_some() && !f.power_limit {
        why.push("asic-rs: power limit not supported on this firmware (supports_set_power_limit() = false)".into());
    }
    if change.tuning_pct.is_some() && !f.tuning_percent {
        why.push("asic-rs: tuning percent not supported on this firmware (supports_set_tuning_percent() = false)".into());
    }
    if change.fan_pct.is_some() && !f.fan {
        why.push("asic-rs: fan config not supported on this firmware".into());
    }
    if change.temp_target_c.is_some() {
        why.push("asic-rs: target temperature is not modelled for this path yet".into());
    }
    if change.auto_fan.is_some() {
        why.push("asic-rs: auto-fan needs a target temperature (not modelled yet)".into());
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(why.join("; "))
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
        ..Default::default()
    }
}

/// The asic-rs calls the settings backend makes. One implementation wraps
/// a real asic-rs miner; mock tests provide per-firmware stand-ins (asic-rs
/// hardcodes real ports, so it cannot reach a local mock).
pub trait NativeTuning: Send + Sync {
    fn flags(&self) -> NativeSettingsFlags;
    /// Power limit currently in force (W), when the firmware reports one.
    fn power_target(&self) -> BoxFuture<'_, Option<u32>>;
    fn set_power_limit(&self, watts: u32) -> BoxFuture<'_, anyhow::Result<bool>>;
    fn tuning_percent(&self) -> BoxFuture<'_, Option<u32>>;
    fn set_tuning_percent(&self, pct: u8) -> BoxFuture<'_, anyhow::Result<bool>>;
    fn fan(&self) -> BoxFuture<'_, Option<FanConfig>>;
    fn set_fan(&self, f: FanConfig) -> BoxFuture<'_, anyhow::Result<bool>>;
}

/// [`NativeTuning`] over a real asic-rs miner.
pub struct AsicRsMiner(pub SharedMiner);

impl NativeTuning for AsicRsMiner {
    fn flags(&self) -> NativeSettingsFlags {
        NativeSettingsFlags {
            power_limit: self.0.supports_set_power_limit(),
            tuning_percent: self.0.supports_set_tuning_percent(),
            fan: self.0.supports_fan_config(),
        }
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
}

impl AsicRsSettings {
    pub fn new(miner: SharedMiner) -> Self {
        Self::from_native(Arc::new(AsicRsMiner(miner)))
    }
    pub fn from_native(native: Arc<dyn NativeTuning>) -> Self {
        Self { native }
    }
}

impl SettingsDriver for AsicRsSettings {
    fn id(&self) -> &'static str {
        "asic-rs"
    }
    fn hardware_confirmed(&self) -> bool {
        false
    }
    fn accepts(&self, _p: &DeviceProfile, change: &Settings) -> Result<(), String> {
        asic_accepts(change, self.native.flags())
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move {
            let f = self.native.flags();
            let fan = if f.fan { self.native.fan().await } else { None };
            let mut s = settings_from_configs(None, fan.as_ref());
            if f.power_limit {
                s.power_limit_w = self.native.power_target().await;
            }
            if f.tuning_percent {
                s.tuning_pct = self.native.tuning_percent().await;
            }
            Ok(s)
        })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let s = s.clone();
        Box::pin(async move {
            if let Some(w) = s.power_limit_w {
                anyhow::ensure!(self.native.set_power_limit(w).await?, "miner refused power limit");
            }
            if let Some(p) = s.tuning_pct {
                let p = u8::try_from(p).map_err(|_| anyhow::anyhow!("tuning_pct {p} out of range"))?;
                anyhow::ensure!(self.native.set_tuning_percent(p).await?, "miner refused tuning percent");
            }
            if let Some(f) = s.fan_pct {
                anyhow::ensure!(self.native.set_fan(FanConfig::Manual { fan_speed: f as u64 }).await?, "miner refused fan config");
            }
            Ok(())
        })
    }
}
