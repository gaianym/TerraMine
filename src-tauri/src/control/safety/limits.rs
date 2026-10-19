//! Per-class / per-model write limits, loaded from `limits.json` (data,
//! not code). A settings change is checked against the entry for the
//! device; no entry, or no range for a field, means the write is refused.

use serde::{Deserialize, Serialize};

use super::profile::{DeviceClass, DeviceProfile};
use super::settings::{Settings, TuningMode};

const BUILTIN: &str = include_str!("limits.json");

pub type Range = [u32; 2];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitEntry {
    pub class: DeviceClass,
    /// Matched case-insensitively against the profile's chip, then model.
    pub model: String,
    #[serde(default)]
    pub frequency_mhz: Option<Range>,
    #[serde(default)]
    pub core_voltage_mv: Option<Range>,
    #[serde(default)]
    pub power_w: Option<Range>,
    #[serde(default)]
    pub fan_pct: Option<Range>,
    /// Auto-fan target temperature (°C).
    #[serde(default)]
    pub temp_target_c: Option<Range>,
    /// asic-rs tuning percent (100 = unthrottled).
    #[serde(default)]
    pub tuning_pct: Option<Range>,
    /// Hashrate target (GH/s).
    #[serde(default)]
    pub tuning_hashrate_ghs: Option<Range>,
    /// Mining modes allowed as a tuning target (absent = none).
    #[serde(default)]
    pub tuning_modes: Option<Vec<TuningMode>>,
    /// Presets may be selected (each must be one the miner lists; a
    /// numeric preset name is a wattage and must sit in `power_w`).
    #[serde(default)]
    pub presets: bool,
    /// Scaling step and minimum (firmware units, ePIC: percent).
    #[serde(default)]
    pub scaling_step: Option<Range>,
    #[serde(default)]
    pub scaling_min: Option<Range>,
    pub max_temp_c: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsTable {
    pub version: u32,
    #[serde(default)]
    pub note: String,
    pub entries: Vec<LimitEntry>,
}

impl LimitsTable {
    pub fn builtin() -> Self {
        Self::from_json(BUILTIN).expect("built-in limits.json is valid")
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        let t: Self = serde_json::from_str(s).map_err(|e| e.to_string())?;
        for e in &t.entries {
            for (name, r) in [
                ("frequency_mhz", e.frequency_mhz),
                ("core_voltage_mv", e.core_voltage_mv),
                ("power_w", e.power_w),
                ("fan_pct", e.fan_pct),
                ("temp_target_c", e.temp_target_c),
                ("tuning_pct", e.tuning_pct),
                ("tuning_hashrate_ghs", e.tuning_hashrate_ghs),
                ("scaling_step", e.scaling_step),
                ("scaling_min", e.scaling_min),
            ] {
                if let Some([lo, hi]) = r {
                    if lo > hi {
                        return Err(format!("{}: {name} range {lo}..{hi} is inverted", e.model));
                    }
                }
            }
        }
        Ok(t)
    }

    pub fn lookup(&self, p: &DeviceProfile) -> Option<&LimitEntry> {
        let eq = |a: &str, b: &str| !a.is_empty() && a.eq_ignore_ascii_case(b.trim());
        let of_class = || self.entries.iter().filter(move |e| e.class == p.class);
        if let Some(chip) = &p.chip {
            if let Some(e) = of_class().find(|e| eq(&e.model, chip)) {
                return Some(e);
            }
        }
        of_class().find(|e| eq(&e.model, &p.model))
    }

    /// Ok(entry) when every field in `change` is within its range.
    pub fn check(&self, p: &DeviceProfile, change: &Settings) -> Result<&LimitEntry, Vec<String>> {
        let Some(e) = self.lookup(p) else {
            return Err(vec![format!(
                "no limits for {:?} {} (chip {}); refusing write",
                p.class,
                p.model,
                p.chip.as_deref().unwrap_or("?")
            )]);
        };
        let mut errs = Vec::new();
        let mut field = |name: &str, v: Option<u32>, r: Option<Range>| match (v, r) {
            (None, _) => {}
            (Some(_), None) => errs.push(format!("{name} is not writable on {}", e.model)),
            (Some(v), Some([lo, hi])) if v < lo || v > hi => {
                errs.push(format!("{name} {v} outside {lo}..={hi} for {}", e.model))
            }
            _ => {}
        };
        field("frequency_mhz", change.frequency_mhz, e.frequency_mhz);
        field("core_voltage_mv", change.core_voltage_mv, e.core_voltage_mv);
        field("power_w", change.power_limit_w, e.power_w);
        field("fan_pct", change.fan_pct, e.fan_pct);
        field("temp_target_c", change.temp_target_c, e.temp_target_c);
        field("tuning_pct", change.tuning_pct, e.tuning_pct);
        field("tuning_hashrate_ghs", change.tuning_hashrate_ghs, e.tuning_hashrate_ghs);
        field("scaling_step", change.scaling_step, e.scaling_step);
        field("scaling_min", change.scaling_min, e.scaling_min);
        if let Some(m) = change.tuning_mode {
            if !e.tuning_modes.as_ref().is_some_and(|l| l.contains(&m)) {
                errs.push(format!("tuning_mode {} is not allowed on {}", m.as_str(), e.model));
            }
        }
        if let Some(p) = &change.tuning_preset {
            if !e.presets {
                errs.push(format!("tuning_preset is not writable on {}", e.model));
            } else if let Ok(w) = p.trim().parse::<u32>() {
                match e.power_w {
                    Some([lo, hi]) if w >= lo && w <= hi => {}
                    Some([lo, hi]) => errs.push(format!("preset {p} ({w} W) outside power_w {lo}..={hi} for {}", e.model)),
                    None => errs.push(format!("preset {p} is a wattage but {} has no power_w range", e.model)),
                }
            }
        }
        // Fan mode needs the device's fan range; thresholds have no range (read only).
        if change.auto_fan.is_some() && e.fan_pct.is_none() {
            errs.push(format!("auto_fan is not writable on {}", e.model));
        }
        for (name, v) in [("temp_hot_c", change.temp_hot_c), ("temp_danger_c", change.temp_danger_c)] {
            if v.is_some() {
                errs.push(format!("{name} is not writable on {}", e.model));
            }
        }
        if errs.is_empty() {
            Ok(e)
        } else {
            Err(errs)
        }
    }
}
