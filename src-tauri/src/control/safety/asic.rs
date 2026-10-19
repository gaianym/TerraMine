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
//! Tuning targets (plan PR D): `set_tuning_config` / `get_tuning_config`
//! (`supports_tuning_config()`). That flag is coarse, so the target kinds
//! each 0.8.6 backend's `set_tuning_config` actually accepts are in
//! [`target_kinds`] (read from the backend sources): stock Antminer 2020+
//! modes only; ePIC power + hashrate (with an algorithm); Proto power;
//! Sealminer power + modes (Low on A2 only); VNish power + presets;
//! Whatsminer v2 / v3 power + modes. A power target keeps going through
//! `set_power_limit` where the backend has it; ePIC (no `set_power_limit`)
//! takes it as a `TuningTarget::Power`. Presets: `get_presets`
//! (`supports_presets()`, VNish); a preset must be one the miner lists.
//! Scaling: `set_scaling_config` / `get_scaling_config`
//! (`supports_scaling_config()`, ePIC only): step and minimum.

use asic_rs_core::config::fan::FanConfig;
use asic_rs_core::config::pools::{PoolConfig, PoolGroupConfig};
use asic_rs_core::config::scaling::ScalingConfig;
use asic_rs_core::config::timezone::TimezoneConfig;
use asic_rs_core::config::tuning::TuningConfig;
use asic_rs_core::data::hashrate::HashRate;
use asic_rs_core::data::miner::MiningMode;
use asic_rs_core::data::pool::{PoolGroupData, PoolURL};
use asic_rs_core::config::temperature::TemperatureConfig;
use asic_rs_core::data::hashrate::HashRateUnit;
use asic_rs_core::data::miner::{MinerData, TuningTarget};
use std::sync::Arc;

use futures::future::BoxFuture;
use measurements::Power;

use super::pools::{normalize_url, parse_timezone, PoolGroupSpec, PoolSpec, Secret};
use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{HealthSample, Settings, SettingsDriver, TuningMode};
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
    /// `supports_tuning_config()`.
    pub tuning_config: bool,
    /// `supports_presets()`.
    pub presets: bool,
    /// `supports_scaling_config()`.
    pub scaling: bool,
}

/// Tuning target kinds a backend's `set_tuning_config` accepts (asic-rs
/// 0.8.6 sources; `supports_tuning_config()` alone does not say which).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TargetKinds {
    /// `TuningTarget::Power` through `set_tuning_config` (used only when
    /// the backend has no `set_power_limit`).
    pub power: bool,
    pub hashrate: bool,
    pub modes: &'static [TuningMode],
    pub preset: bool,
    /// Hashrate targets need an algorithm (ePIC).
    pub needs_algorithm: bool,
}

const ALL_MODES: &[TuningMode] = &[TuningMode::Low, TuningMode::Normal, TuningMode::High];
const NO_LOW: &[TuningMode] = &[TuningMode::Normal, TuningMode::High];

/// Default algorithm for an ePIC hashrate target when none is given.
pub const EPIC_HASHRATE_ALGORITHM: &str = "ChipTune";

pub fn target_kinds(p: &DeviceProfile) -> TargetKinds {
    let none = TargetKinds::default();
    match p.class {
        DeviceClass::AntminerStock => TargetKinds { modes: ALL_MODES, ..none },
        DeviceClass::Epic => TargetKinds { power: true, hashrate: true, needs_algorithm: true, ..none },
        DeviceClass::Proto => TargetKinds { power: true, ..none },
        DeviceClass::Sealminer => {
            let a2 = p.model.trim().eq_ignore_ascii_case("a2") || p.model.to_ascii_lowercase().ends_with(" a2");
            TargetKinds { power: true, modes: if a2 { ALL_MODES } else { NO_LOW }, ..none }
        }
        DeviceClass::Vnish => TargetKinds { power: true, preset: true, ..none },
        DeviceClass::Whatsminer => TargetKinds { power: true, modes: ALL_MODES, ..none },
        _ => none,
    }
}

fn mining_mode(m: TuningMode) -> MiningMode {
    match m {
        TuningMode::Low => MiningMode::Low,
        TuningMode::Normal => MiningMode::Normal,
        TuningMode::High => MiningMode::High,
    }
}

/// The tuning target of `s` as one asic-rs `TuningConfig` (power only when
/// `power_via_config`).
pub fn tuning_config_for(s: &Settings, power_via_config: bool) -> Option<TuningConfig> {
    let target = if let Some(m) = s.tuning_mode {
        TuningTarget::MiningMode(mining_mode(m))
    } else if let Some(h) = s.tuning_hashrate_ghs {
        TuningTarget::HashRate(HashRate { value: f64::from(h), unit: HashRateUnit::GigaHash, algo: asic_rs_core::data::device::HashAlgorithm::SHA256 })
    } else if let Some(p) = &s.tuning_preset {
        TuningTarget::Preset(p.clone())
    } else if let (Some(w), true) = (s.power_limit_w, power_via_config) {
        TuningTarget::Power(Power::from_watts(f64::from(w)))
    } else {
        return None;
    };
    let mut c = TuningConfig::new(target);
    if let Some(a) = s.tuning_algorithm.as_ref().filter(|a| !a.is_empty()) {
        c = c.with_algorithm(a.clone());
    }
    Some(c)
}

/// Settings fields for a reported `TuningConfig` (one target kind).
pub fn settings_from_tuning(c: &TuningConfig) -> Settings {
    let mut s = Settings { tuning_algorithm: c.algorithm.clone(), ..Default::default() };
    match &c.target {
        TuningTarget::Power(p) => s.power_limit_w = Some(p.as_watts().round() as u32),
        TuningTarget::HashRate(h) => s.tuning_hashrate_ghs = Some(h.clone().as_unit(HashRateUnit::GigaHash).value.round() as u32),
        TuningTarget::MiningMode(m) => {
            s.tuning_mode = Some(match m {
                MiningMode::Low => TuningMode::Low,
                MiningMode::High => TuningMode::High,
                _ => TuningMode::Normal,
            })
        }
        TuningTarget::Preset(n) => s.tuning_preset = Some(n.clone()),
        TuningTarget::Manual { .. } => {}
    }
    s
}

/// Tuning target / preset / scaling checks (plan PR D), no I/O.
pub fn target_accepts(change: &Settings, f: NativeSettingsFlags, k: TargetKinds) -> Vec<String> {
    let mut why = Vec::new();
    if change.target_kinds() > 1 {
        why.push("a miner runs one tuning target: send one of power, hashrate, mode or preset".to_string());
        return why;
    }
    let cfg = |why: &mut Vec<String>, what: &str, ok: bool| {
        if !f.tuning_config {
            why.push(format!("asic-rs: {what} not supported on this firmware (supports_tuning_config() = false)"));
        } else if !ok {
            why.push(format!("asic-rs: this firmware's set_tuning_config does not take a {what}"));
        }
    };
    if let Some(m) = change.tuning_mode {
        let ok = k.modes.contains(&m);
        if f.tuning_config && !k.modes.is_empty() && !ok {
            why.push(format!("asic-rs: mining mode {} not available on this model", m.as_str()));
        } else {
            cfg(&mut why, "mining mode target", ok);
        }
    }
    if change.tuning_hashrate_ghs.is_some() {
        cfg(&mut why, "hashrate target", k.hashrate);
    }
    if change.tuning_preset.is_some() {
        if !f.presets {
            why.push("asic-rs: presets not supported on this firmware (supports_presets() = false)".into());
        } else {
            cfg(&mut why, "preset target", k.preset);
        }
    }
    if change.tuning_algorithm.is_some() && !(k.needs_algorithm && f.tuning_config) {
        why.push("asic-rs: this firmware takes no tuning algorithm".into());
    }
    if change.touches_scaling() {
        if !f.scaling {
            why.push("asic-rs: scaling not supported on this firmware (supports_scaling_config() = false)".into());
        } else if change.scaling_step.is_none() || change.scaling_min.is_none() {
            why.push("scaling needs both scaling_step and scaling_min".into());
        }
    }
    why
}

/// Settings fields asic-rs can write, given its support flags.
pub fn asic_accepts(change: &Settings, f: NativeSettingsFlags) -> Result<(), String> {
    asic_accepts_with(change, f, TargetKinds::default())
}

/// [`asic_accepts`] plus the tuning target kinds of the device.
pub fn asic_accepts_with(change: &Settings, f: NativeSettingsFlags, k: TargetKinds) -> Result<(), String> {
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
    if change.power_limit_w.is_some() && !f.power_limit && !(f.tuning_config && k.power) {
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
    /// Current tuning target, when `flags().tuning_config`.
    fn tuning_config(&self) -> BoxFuture<'_, Option<TuningConfig>> {
        Box::pin(async { None })
    }
    fn set_tuning_config(&self, _c: TuningConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("Setting tuning config is not supported on this platform") })
    }
    /// Preset names the firmware lists, when `flags().presets`.
    fn presets(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn scaling(&self) -> BoxFuture<'_, Option<ScalingConfig>> {
        Box::pin(async { None })
    }
    fn set_scaling(&self, _c: ScalingConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async { anyhow::bail!("Setting scaling config is not supported on this platform") })
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
            tuning_config: self.0.supports_tuning_config(),
            presets: self.0.supports_presets(),
            scaling: self.0.supports_scaling_config(),
        }
    }
    fn tuning_config(&self) -> BoxFuture<'_, Option<TuningConfig>> {
        Box::pin(async move { self.0.get_tuning_config().await.ok() })
    }
    fn set_tuning_config(&self, c: TuningConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_tuning_config(c, None).await })
    }
    fn presets(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move { self.0.get_presets().await.into_iter().map(|p| p.name).collect() })
    }
    fn scaling(&self) -> BoxFuture<'_, Option<ScalingConfig>> {
        Box::pin(async move { self.0.get_scaling_config().await.ok() })
    }
    fn set_scaling(&self, c: ScalingConfig) -> BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move { self.0.set_scaling_config(c).await })
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
    /// Tuning target kinds of this device (from its discovery profile).
    kinds: TargetKinds,
}

impl AsicRsSettings {
    pub fn new(miner: SharedMiner) -> Self {
        Self::from_native(Arc::new(AsicRsMiner(miner)))
    }
    pub fn from_native(native: Arc<dyn NativeTuning>) -> Self {
        Self { native, idle_speed: std::sync::Mutex::new(None), kinds: TargetKinds::default() }
    }
    /// Tuning target kinds for this device (see [`target_kinds`]).
    pub fn with_profile(mut self, p: &DeviceProfile) -> Self {
        self.kinds = target_kinds(p);
        self
    }
    /// A power target goes through `set_tuning_config` (no `set_power_limit`).
    fn power_via_config(&self) -> bool {
        let f = self.native.flags();
        !f.power_limit && f.tuning_config && self.kinds.power
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
        let k = target_kinds(p);
        let f = NativeSettingsFlags { auto_target, ..f };
        let mut why: Vec<String> = asic_accepts_with(change, f, k).err().into_iter().collect();
        why.extend(target_accepts(change, f, k));
        if why.is_empty() {
            Ok(())
        } else {
            Err(why.join("; "))
        }
    }
    fn presets(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move { if self.native.flags().presets { self.native.presets().await } else { Vec::new() } })
    }
    /// Fill the ePIC default algorithm for a hashrate target, check a
    /// preset against the miner's own list, and a timezone against the
    /// zones the miner lists (when it lists any).
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
            let mut out = change.clone();
            let mut note = None;
            if change.tuning_hashrate_ghs.is_some() && self.kinds.needs_algorithm && change.tuning_algorithm.is_none() {
                out.tuning_algorithm = Some(EPIC_HASHRATE_ALGORITHM.into());
                note = Some(format!("hashrate target runs under the {EPIC_HASHRATE_ALGORITHM} algorithm"));
            }
            if let Some(p) = &change.tuning_preset {
                let list = self.native.presets().await;
                if !list.iter().any(|n| n == p) {
                    let have = if list.is_empty() { "none reported".to_string() } else { list.join(", ") };
                    return Err(format!("preset {p:?} is not one this miner offers (available: {have})"));
                }
            }
            Ok((out, note))
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
            if f.tuning_config {
                if let Some(c) = self.native.tuning_config().await {
                    let t = settings_from_tuning(&c);
                    // The tuning config names the one running target; the
                    // power field then comes from it, not a stale limit.
                    if t.touches_tuning_target() {
                        s.power_limit_w = t.power_limit_w;
                    }
                    s.tuning_mode = t.tuning_mode;
                    s.tuning_hashrate_ghs = t.tuning_hashrate_ghs;
                    s.tuning_preset = t.tuning_preset;
                    s.tuning_algorithm = t.tuning_algorithm;
                }
            }
            if f.scaling {
                if let Some(c) = self.native.scaling().await {
                    s.scaling_step = Some(c.step);
                    s.scaling_min = Some(c.minimum);
                }
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
            if s.touches_scaling() {
                let (Some(step), Some(min)) = (s.scaling_step, s.scaling_min) else {
                    anyhow::bail!("scaling needs both step and minimum");
                };
                anyhow::ensure!(self.native.set_scaling(ScalingConfig::new(step, min)).await?, "miner refused scaling config");
            }
            let via_config = self.power_via_config();
            if let Some(w) = s.power_limit_w.filter(|_| !via_config) {
                anyhow::ensure!(self.native.set_power_limit(w).await?, "miner refused power limit");
            } else if let Some(c) = tuning_config_for(&s, via_config) {
                anyhow::ensure!(self.native.set_tuning_config(c).await?, "miner refused tuning target");
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
