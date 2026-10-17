//! Per-class / per-model write limits, loaded from `limits.json` (data,
//! not code). A settings change is checked against the entry for the
//! device; no entry, or no range for a field, means the write is refused.

use serde::{Deserialize, Serialize};

use super::profile::{DeviceClass, DeviceProfile};
use super::settings::Settings;

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
        if errs.is_empty() {
            Ok(e)
        } else {
            Err(errs)
        }
    }
}
