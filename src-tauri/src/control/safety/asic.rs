//! asic-rs (0.4.2) backed settings and health. Writes go through the
//! typed asic-rs setters and are gated by its `supports_*()` flags;
//! snapshot / read-back use the matching asic-rs getters.

use asic_rs_core::config::fan::FanConfig;
use asic_rs_core::config::tuning::TuningConfig;
use asic_rs_core::data::hashrate::HashRateUnit;
use asic_rs_core::data::miner::{MinerData, TuningTarget};
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

/// Settings fields asic-rs 0.4.2 can write, given its support flags.
pub fn asic_accepts(change: &Settings, tuning: bool, power_limit: bool, fan: bool) -> Result<(), String> {
    if change.frequency_mhz.is_some() || change.core_voltage_mv.is_some() {
        return Err("asic-rs has no manual frequency/voltage target".into());
    }
    if change.power_limit_w.is_some() && !(tuning || power_limit) {
        return Err("asic-rs: power target not supported on this firmware".into());
    }
    if change.fan_pct.is_some() && !fan {
        return Err("asic-rs: fan config not supported on this firmware".into());
    }
    if change.auto_fan.is_some() {
        return Err("asic-rs: auto-fan needs a target temperature (not modelled yet)".into());
    }
    Ok(())
}

pub fn settings_from_configs(tuning: Option<&TuningConfig>, fan: Option<&FanConfig>) -> Settings {
    Settings {
        power_limit_w: tuning.and_then(|t| match &t.target {
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

pub struct AsicRsSettings {
    miner: SharedMiner,
}

impl AsicRsSettings {
    pub fn new(miner: SharedMiner) -> Self {
        Self { miner }
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
        let m = &self.miner;
        asic_accepts(change, m.supports_tuning_config(), m.supports_set_power_limit(), m.supports_fan_config())
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async move {
            let tuning = if self.miner.supports_tuning_config() { self.miner.get_tuning_config().await.ok() } else { None };
            let fan = if self.miner.supports_fan_config() { self.miner.get_fan_config().await.ok() } else { None };
            Ok(settings_from_configs(tuning.as_ref(), fan.as_ref()))
        })
    }
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        let s = s.clone();
        Box::pin(async move {
            if let Some(w) = s.power_limit_w {
                let p = Power::from_watts(w as f64);
                let ok = if self.miner.supports_tuning_config() {
                    self.miner.set_tuning_config(TuningConfig::new(TuningTarget::Power(p)), None).await?
                } else {
                    self.miner.set_power_limit(p).await?
                };
                anyhow::ensure!(ok, "miner refused power target");
            }
            if let Some(f) = s.fan_pct {
                anyhow::ensure!(self.miner.set_fan_config(FanConfig::Manual { fan_speed: f as u64 }).await?, "miner refused fan config");
            }
            Ok(())
        })
    }
}
