//! Writable settings and the per-device driver interface the safety
//! engine uses for snapshot / write / read-back / health.

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// A partial set of settings. `None` = not part of this change / unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_voltage_mv: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_pct: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_fan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_w: Option<u32>,
}

impl Settings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Keep only the fields that are set in `mask` (what a snapshot needs
    /// to restore for a given change).
    pub fn restrict_to(&self, mask: &Settings) -> Settings {
        Settings {
            frequency_mhz: mask.frequency_mhz.and(self.frequency_mhz),
            core_voltage_mv: mask.core_voltage_mv.and(self.core_voltage_mv),
            fan_pct: mask.fan_pct.and(self.fan_pct),
            auto_fan: mask.auto_fan.and(self.auto_fan),
            power_limit_w: mask.power_limit_w.and(self.power_limit_w),
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
        cmp(&mut out, "frequency_mhz", self.frequency_mhz, want.frequency_mhz);
        cmp(&mut out, "core_voltage_mv", self.core_voltage_mv, want.core_voltage_mv);
        cmp(&mut out, "fan_pct", self.fan_pct, want.fan_pct);
        cmp(&mut out, "auto_fan", self.auto_fan, want.auto_fan);
        cmp(&mut out, "power_limit_w", self.power_limit_w, want.power_limit_w);
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
        f("frequency_mhz", s(self.frequency_mhz), s(to.frequency_mhz));
        f("core_voltage_mv", s(self.core_voltage_mv), s(to.core_voltage_mv));
        f("fan_pct", s(self.fan_pct), s(to.fan_pct));
        f("auto_fan", self.auto_fan.map(|b| b.to_string()), to.auto_fan.map(|b| b.to_string()));
        f("power_limit_w", s(self.power_limit_w), s(to.power_limit_w));
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

/// A device opened by a write path.
pub trait SettingsDriver: Send + Sync {
    fn read(&self) -> BoxFuture<'_, anyhow::Result<Settings>>;
    fn write(&self, s: &Settings) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Never errors: an unreachable device is a sample, not a failure.
    fn health(&self) -> BoxFuture<'_, HealthSample>;
}
