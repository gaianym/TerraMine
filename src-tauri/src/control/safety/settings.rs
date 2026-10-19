//! Writable settings and the per-device driver interface the safety
//! engine uses for snapshot / write / read-back / health.

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::pools::{PoolGroupSpec, PoolOp};
/// A named firmware mining mode (asic-rs `MiningMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuningMode {
    Low,
    Normal,
    High,
}

impl TuningMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

/// A partial set of settings. `None` = not part of this change / unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Pools (plan PR F), the full list. A request usually carries
    /// [`Self::pool_op`] instead; each backend resolves it to this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Vec<PoolGroupSpec>>,
    /// Pool operation (set / add / replace), resolved per miner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_op: Option<PoolOp>,
    /// IANA timezone, e.g. "America/Chicago" (asic-rs `TimezoneConfig`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_voltage_mv: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_pct: Option<u32>,
    /// Auto-fan target chip temperature (°C), AxeOS `temptarget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_target_c: Option<u32>,
    /// Thermal thresholds (°C): fans at 100% / thermal shutdown. asic-rs
    /// `TemperatureConfig` `hot` / `danger`; read only in asic-rs 0.8.6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_hot_c: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_danger_c: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_fan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_w: Option<u32>,
    /// Manual tuning percent of full power (100 = unthrottled), asic-rs
    /// `set_tuning_percent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning_pct: Option<u32>,
    /// Tuning target: a named mining mode (asic-rs `TuningTarget::MiningMode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning_mode: Option<TuningMode>,
    /// Tuning target: hashrate in GH/s (asic-rs `TuningTarget::HashRate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning_hashrate_ghs: Option<u32>,
    /// Tuning target: a named firmware preset (asic-rs `TuningTarget::Preset`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning_preset: Option<String>,
    /// Tuning algorithm the target runs under (ePIC `ChipTune`, `PowerTune`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning_algorithm: Option<String>,
    /// Scaling (asic-rs `ScalingConfig`): throttle step and minimum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaling_step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaling_min: Option<u32>,
}

impl Settings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A pool field (list or operation) is set.
    pub fn touches_pools(&self) -> bool {
        self.pools.is_some() || self.pool_op.is_some()
    }

    /// Any fan field (manual %, auto, target temperature) is set.
    pub fn touches_fan(&self) -> bool {
        self.fan_pct.is_some() || self.auto_fan.is_some() || self.temp_target_c.is_some()
    }

    /// A tuning target field (power, hashrate, mode, preset) is set.
    pub fn touches_tuning_target(&self) -> bool {
        self.power_limit_w.is_some()
            || self.tuning_hashrate_ghs.is_some()
            || self.tuning_mode.is_some()
            || self.tuning_preset.is_some()
    }

    /// Fields of the D-plan tuning-target / scaling kinds (not power).
    pub fn touches_target_or_scaling(&self) -> bool {
        self.tuning_hashrate_ghs.is_some()
            || self.tuning_mode.is_some()
            || self.tuning_preset.is_some()
            || self.tuning_algorithm.is_some()
            || self.touches_scaling()
    }

    pub fn touches_scaling(&self) -> bool {
        self.scaling_step.is_some() || self.scaling_min.is_some()
    }

    /// How many tuning target kinds are set (a firmware runs one target).
    pub fn target_kinds(&self) -> usize {
        [
            self.power_limit_w.is_some(),
            self.tuning_hashrate_ghs.is_some(),
            self.tuning_mode.is_some(),
            self.tuning_preset.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }

    /// `self` plus every tuning target field (and the algorithm) when any
    /// is set, and both scaling fields when either is: the firmware runs
    /// one target, so a rollback restores whichever was active before.
    pub fn with_tuning_group(&self) -> Settings {
        let mut m = self.clone();
        if self.touches_tuning_target() {
            m.power_limit_w = m.power_limit_w.or(Some(0));
            m.tuning_hashrate_ghs = m.tuning_hashrate_ghs.or(Some(0));
            m.tuning_mode = m.tuning_mode.or(Some(TuningMode::Normal));
            m.tuning_preset = m.tuning_preset.or(Some(String::new()));
            m.tuning_algorithm = m.tuning_algorithm.or(Some(String::new()));
        }
        if self.touches_scaling() {
            m.scaling_step = m.scaling_step.or(Some(0));
            m.scaling_min = m.scaling_min.or(Some(0));
        }
        m
    }

    /// `self` plus every fan field when any is set: a fan change snapshots
    /// the whole fan state (mode + manual % + target), so a rollback
    /// restores the mode too.
    pub fn with_fan_group(&self) -> Settings {
        let mut m = self.clone();
        if self.touches_fan() {
            m.fan_pct = m.fan_pct.or(Some(0));
            m.auto_fan = m.auto_fan.or(Some(false));
            m.temp_target_c = m.temp_target_c.or(Some(0));
        }
        m
    }

    /// Keep only the fields that are set in `mask` (what a snapshot needs
    /// to restore for a given change).
    pub fn restrict_to(&self, mask: &Settings) -> Settings {
        Settings {
            // A pool operation is restored as the full previous list.
            pools: (mask.pools.is_some() || mask.pool_op.is_some()).then(|| self.pools.clone()).flatten(),
            pool_op: None,
            timezone: mask.timezone.as_ref().and(self.timezone.clone()),
            frequency_mhz: mask.frequency_mhz.and(self.frequency_mhz),
            core_voltage_mv: mask.core_voltage_mv.and(self.core_voltage_mv),
            fan_pct: mask.fan_pct.and(self.fan_pct),
            temp_target_c: mask.temp_target_c.and(self.temp_target_c),
            temp_hot_c: mask.temp_hot_c.and(self.temp_hot_c),
            temp_danger_c: mask.temp_danger_c.and(self.temp_danger_c),
            auto_fan: mask.auto_fan.and(self.auto_fan),
            power_limit_w: mask.power_limit_w.and(self.power_limit_w),
            tuning_pct: mask.tuning_pct.and(self.tuning_pct),
            tuning_mode: mask.tuning_mode.and(self.tuning_mode),
            tuning_hashrate_ghs: mask.tuning_hashrate_ghs.and(self.tuning_hashrate_ghs),
            tuning_preset: mask.tuning_preset.as_ref().and(self.tuning_preset.clone()),
            tuning_algorithm: mask.tuning_algorithm.as_ref().and(self.tuning_algorithm.clone()),
            scaling_step: mask.scaling_step.and(self.scaling_step),
            scaling_min: mask.scaling_min.and(self.scaling_min),
        }
    }

    /// Fields set in `want` whose value in `self` differs ("field: got X, want Y").
    pub fn mismatches(&self, want: &Settings) -> Vec<String> {
        fn cmp<T: PartialEq + std::fmt::Debug>(out: &mut Vec<String>, n: &str, got: Option<T>, want: Option<T>) {
            if let Some(w) = want {
                if got.as_ref() != Some(&w) {
                    out.push(format!("{n}: got {got:?}, want {w:?}"));
                }
            }
        }
        let mut out = Vec::new();
        if let Some(w) = &want.pools {
            match &self.pools {
                Some(g) if super::pools::same_pools(g, w) => {}
                got => out.push(format!(
                    "pools: got {}, want {}",
                    got.as_deref().map_or("?".into(), super::pools::describe),
                    super::pools::describe(w)
                )),
            }
        }
        let tz = |v: &Option<String>| v.as_ref().map(|t| t.trim().to_string());
        cmp(&mut out, "timezone", tz(&self.timezone), tz(&want.timezone));
        cmp(&mut out, "frequency_mhz", self.frequency_mhz, want.frequency_mhz);
        cmp(&mut out, "core_voltage_mv", self.core_voltage_mv, want.core_voltage_mv);
        cmp(&mut out, "fan_pct", self.fan_pct, want.fan_pct);
        cmp(&mut out, "temp_target_c", self.temp_target_c, want.temp_target_c);
        cmp(&mut out, "temp_hot_c", self.temp_hot_c, want.temp_hot_c);
        cmp(&mut out, "temp_danger_c", self.temp_danger_c, want.temp_danger_c);
        cmp(&mut out, "auto_fan", self.auto_fan, want.auto_fan);
        cmp(&mut out, "power_limit_w", self.power_limit_w, want.power_limit_w);
        cmp(&mut out, "tuning_pct", self.tuning_pct, want.tuning_pct);
        cmp(&mut out, "tuning_mode", self.tuning_mode, want.tuning_mode);
        cmp(&mut out, "tuning_hashrate_ghs", self.tuning_hashrate_ghs, want.tuning_hashrate_ghs);
        cmp(&mut out, "tuning_preset", self.tuning_preset.clone(), want.tuning_preset.clone());
        // Firmware reports algorithm names in its own case.
        let lc = |v: &Option<String>| v.as_ref().map(|a| a.to_ascii_lowercase());
        cmp(&mut out, "tuning_algorithm", lc(&self.tuning_algorithm), lc(&want.tuning_algorithm));
        cmp(&mut out, "scaling_step", self.scaling_step, want.scaling_step);
        cmp(&mut out, "scaling_min", self.scaling_min, want.scaling_min);
        out
    }

    /// Human-readable "from → to" diff for dry runs.
    pub fn describe_change(&self, to: &Settings) -> String {
        let mut parts = Vec::new();
        let mut f = |n: &str, a: Option<String>, b: Option<String>| {
            if let Some(b) = b {
                parts.push(format!("{n} {} → {b}", a.unwrap_or_else(|| "?".into())));
            }
        };
        let s = |v: Option<u32>| v.map(|x| x.to_string());
        let pl = |v: &Option<Vec<PoolGroupSpec>>| v.as_deref().map(super::pools::describe);
        f("pools", pl(&self.pools), pl(&to.pools));
        f("timezone", self.timezone.clone(), to.timezone.clone());
        f("frequency_mhz", s(self.frequency_mhz), s(to.frequency_mhz));
        f("core_voltage_mv", s(self.core_voltage_mv), s(to.core_voltage_mv));
        f("fan_pct", s(self.fan_pct), s(to.fan_pct));
        f("temp_target_c", s(self.temp_target_c), s(to.temp_target_c));
        f("temp_hot_c", s(self.temp_hot_c), s(to.temp_hot_c));
        f("temp_danger_c", s(self.temp_danger_c), s(to.temp_danger_c));
        f("auto_fan", self.auto_fan.map(|b| b.to_string()), to.auto_fan.map(|b| b.to_string()));
        f("power_limit_w", s(self.power_limit_w), s(to.power_limit_w));
        f("tuning_pct", s(self.tuning_pct), s(to.tuning_pct));
        let m = |v: Option<TuningMode>| v.map(|x| x.as_str().to_string());
        f("tuning_mode", m(self.tuning_mode), m(to.tuning_mode));
        f("tuning_hashrate_ghs", s(self.tuning_hashrate_ghs), s(to.tuning_hashrate_ghs));
        f("tuning_preset", self.tuning_preset.clone(), to.tuning_preset.clone());
        f("tuning_algorithm", self.tuning_algorithm.clone(), to.tuning_algorithm.clone());
        f("scaling_step", s(self.scaling_step), s(to.scaling_step));
        f("scaling_min", s(self.scaling_min), s(to.scaling_min));
        parts.join(", ")
    }
}

/// One health observation after a change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthSample {
    pub reachable: bool,
    pub hashrate_ghs: Option<f64>,
    pub temp_c: Option<f64>,
}

impl HealthSample {
    pub fn unreachable() -> Self {
        Self { reachable: false, hashrate_ghs: None, temp_c: None }
    }
}

/// A path that exists but deliberately writes nothing; it only explains why
/// (shown in "paths tried").
pub struct NoSettingsPath {
    pub id: &'static str,
    pub reason: &'static str,
}

impl SettingsDriver for NoSettingsPath {
    fn id(&self) -> &'static str {
        self.id
    }
    fn hardware_confirmed(&self) -> bool {
        false
    }
    fn accepts(&self, _p: &super::profile::DeviceProfile, _change: &Settings) -> Result<(), String> {
        Err(self.reason.to_string())
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>> {
        Box::pin(async { anyhow::bail!("no settings path") })
    }
    fn write(&self, _s: &Settings) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async { anyhow::bail!("no settings path") })
    }
}

/// A settings write backend for one device (asic-rs or the AxeOS override).
pub trait SettingsDriver: Send + Sync {
    fn id(&self) -> &'static str;
    /// False until verified against real hardware; such backends are only
    /// used when the caller explicitly allows it (mock tests).
    fn hardware_confirmed(&self) -> bool;
    /// Confirmed on real hardware for this particular change (e.g. AxeOS
    /// 2.15.x frequency and fan, but not core voltage). Default: the
    /// backend-wide [`Self::hardware_confirmed`].
    fn confirmed_for(&self, _change: &Settings) -> bool {
        self.hardware_confirmed()
    }
    /// Static check, no I/O. Err = reason this backend cannot do the change.
    fn accepts(&self, p: &super::profile::DeviceProfile, change: &Settings) -> Result<(), String>;
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>>;
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Rewrite a request into what this backend actually writes (e.g. the
    /// AxeOS power stand-in), plus a note for the plan text. Default: as is.
    fn translate<'a>(&'a self, change: &'a Settings) -> BoxFuture<'a, Result<(Settings, Option<String>), String>> {
        Box::pin(async move { Ok((change.clone(), None)) })
    }
    /// A restart this backend needs for `change` to take effect (AxeOS
    /// frequency / core voltage); `None` = applies without a restart.
    fn restart_to_apply(&self, _change: &Settings) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        None
    }
    /// Seconds since boot, used to prove a restart happened.
    fn uptime_s(&self) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
        Box::pin(async { Ok(None) })
    }
    /// Named presets the firmware offers (empty = none / not supported).
    fn presets(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    /// Beyond the configured values read back by [`Self::read`]: is the
    /// change actually running (e.g. AxeOS `actualFrequency`)? Default: yes.
    fn check_applied<'a>(&'a self, _want: &'a Settings) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}
