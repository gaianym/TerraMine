//! Live capability matrix (plan step I): for each miner, every action
//! TerraMine knows, whether it is supported, by which path, every path
//! considered, and whether the path is hardware-confirmed (live) or dry-run
//! only. Nothing here is hard-coded per firmware: control actions come from
//! the driver's [`Capabilities`] and `paths_tried`, settings from the
//! safety engine's own path selection ([`SafetyEngine::select`] and
//! `confirmed_for`), and the raw asic-rs `supports_*()` flags from the
//! miner object discovery cached. The matrix is a read: nothing is written,
//! and the only network I/O is the identity check the resolver already does.

use std::net::IpAddr;
use std::sync::Arc;

use serde::Serialize;

use super::driver::{MinerAction, MinerDriver};
use super::safety::pools::{PoolOp, PoolSpec};
use super::safety::settings::{Settings, TuningMode};
use super::safety::{DeviceClass, SafetyEngine, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    /// Supported and confirmed on real hardware: a live run is allowed.
    Confirmed,
    /// Supported, but the path is not hardware-confirmed: dry run only.
    DryRunOnly,
    /// No path accepts this action on this miner.
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixCell {
    /// Stable id (`start`, `power_limit`, ...).
    pub action: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub status: CellStatus,
    /// The route / backend that would be used.
    pub path: Option<String>,
    /// Every path considered and why it was or was not usable.
    pub paths_tried: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeFlag {
    pub name: &'static str,
    pub supported: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixRow {
    pub ip: String,
    pub found: bool,
    pub class: Option<DeviceClass>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    /// Driver family that handles the miner (`asic-rs`, `axeos`, `s9-stock`).
    pub driver: Option<&'static str>,
    pub cells: Vec<MatrixCell>,
    /// Raw asic-rs `supports_*()` flags, when discovery kept the miner object.
    pub native: Vec<NativeFlag>,
    pub error: Option<String>,
}

/// One representative change per settings action: the engine decides
/// whether, and by which backend, it would be accepted.
pub fn settings_probes() -> Vec<(&'static str, &'static str, Settings)> {
    let s = Settings::default;
    vec![
        (
            "power_limit",
            "Power limit",
            Settings {
                power_limit_w: Some(1),
                ..s()
            },
        ),
        (
            "tuning_percent",
            "Tuning percent",
            Settings {
                tuning_pct: Some(100),
                ..s()
            },
        ),
        (
            "tuning_mode",
            "Tuning mode",
            Settings {
                tuning_mode: Some(TuningMode::Low),
                ..s()
            },
        ),
        (
            "tuning_hashrate",
            "Hashrate target",
            Settings {
                tuning_hashrate_ghs: Some(1),
                ..s()
            },
        ),
        (
            "tuning_preset",
            "Tuning preset",
            Settings {
                tuning_preset: Some(String::new()),
                ..s()
            },
        ),
        (
            "scaling",
            "Scaling",
            Settings {
                scaling_step: Some(1),
                scaling_min: Some(1),
                ..s()
            },
        ),
        (
            "frequency",
            "Frequency",
            Settings {
                frequency_mhz: Some(1),
                ..s()
            },
        ),
        (
            "core_voltage",
            "Core voltage",
            Settings {
                core_voltage_mv: Some(1),
                ..s()
            },
        ),
        (
            "fan_manual",
            "Fan (manual %)",
            Settings {
                auto_fan: Some(false),
                fan_pct: Some(100),
                ..s()
            },
        ),
        (
            "fan_auto",
            "Fan (auto)",
            Settings {
                auto_fan: Some(true),
                ..s()
            },
        ),
        (
            "temp_target",
            "Fan target temperature",
            Settings {
                auto_fan: Some(true),
                temp_target_c: Some(60),
                ..s()
            },
        ),
        (
            "pools",
            "Pools",
            Settings {
                pool_op: Some(PoolOp::Add {
                    pool: PoolSpec {
                        url: "stratum+tcp://probe:1".into(),
                        user: "probe".into(),
                        password: Default::default(),
                    },
                }),
                ..s()
            },
        ),
        (
            "timezone",
            "Timezone",
            Settings {
                timezone: Some("UTC".into()),
                ..s()
            },
        ),
    ]
}

fn action_label(a: MinerAction) -> &'static str {
    match a {
        MinerAction::Start => "Start",
        MinerAction::Stop => "Stop",
        MinerAction::Reboot => "Reboot",
        MinerAction::LightOn => "Fault light on",
        MinerAction::LightOff => "Fault light off",
    }
}

/// The asic-rs 0.8.6 `supports_*()` flags of a miner object.
pub fn native_flags(m: &dyn asic_rs_core::traits::miner::Miner) -> Vec<NativeFlag> {
    let f = |name, supported| NativeFlag { name, supported };
    vec![
        f("supports_resume", m.supports_resume()),
        f("supports_pause", m.supports_pause()),
        f("supports_restart", m.supports_restart()),
        f("supports_set_fault_light", m.supports_set_fault_light()),
        f("supports_set_power_limit", m.supports_set_power_limit()),
        f(
            "supports_set_tuning_percent",
            m.supports_set_tuning_percent(),
        ),
        f("supports_tuning_config", m.supports_tuning_config()),
        f("supports_presets", m.supports_presets()),
        f("supports_scaling_config", m.supports_scaling_config()),
        f("supports_reset_scaling", m.supports_reset_scaling()),
        f("supports_fan_config", m.supports_fan_config()),
        f(
            "supports_temperature_config",
            m.supports_temperature_config(),
        ),
        f("supports_pools_config", m.supports_pools_config()),
        f("supports_timezone_config", m.supports_timezone_config()),
        f(
            "supports_set_hashboards_enabled",
            m.supports_set_hashboards_enabled(),
        ),
        f("supports_read_logs", m.supports_read_logs()),
        f("supports_change_password", m.supports_change_password()),
        f("supports_factory_reset", m.supports_factory_reset()),
        f("supports_restore_stock_os", m.supports_restore_stock_os()),
        f("supports_upgrade_firmware", m.supports_upgrade_firmware()),
        f("supports_prepare_firmware", m.supports_prepare_firmware()),
        f(
            "supports_check_firmware_update",
            m.supports_check_firmware_update(),
        ),
    ]
}

/// Control-action cells straight from the driver's capability gate.
pub fn control_cells(d: &dyn MinerDriver) -> Vec<MatrixCell> {
    let caps = d.caps();
    MinerAction::ALL
        .into_iter()
        .map(|a| {
            let route = caps.route(a);
            let status = match route {
                None => CellStatus::Unsupported,
                Some(_) if caps.confirmed(a) => CellStatus::Confirmed,
                Some(_) => CellStatus::DryRunOnly,
            };
            let mut tried = Vec::new();
            if let Some(r) = route {
                let note = if status == CellStatus::Confirmed {
                    "selected"
                } else {
                    "selected for dry run only (not yet confirmed on real hardware)"
                };
                tried.push(format!(
                    "{}: {note}",
                    serde_json::to_value(r)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_else(|| format!("{r:?}"))
                ));
            }
            tried.extend(d.paths_tried(a));
            MatrixCell {
                action: a.name(),
                group: "control",
                label: action_label(a),
                status,
                path: route.map(|r| {
                    serde_json::to_value(r)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_else(|| format!("{r:?}"))
                }),
                paths_tried: tried,
            }
        })
        .collect()
}

/// Settings cells from the engine's path selection (dry-run view, so an
/// unconfirmed backend still shows which path it would be).
pub fn settings_cells(engine: &SafetyEngine, t: &Target) -> Vec<MatrixCell> {
    settings_probes()
        .into_iter()
        .map(|(id, label, change)| {
            let (b, tried) = engine.select(t, &change, true);
            let (status, path) = match &b {
                None => (CellStatus::Unsupported, None),
                Some(b) if b.confirmed_for(&change) || engine.opts.allow_unconfirmed_paths => {
                    (CellStatus::Confirmed, Some(b.id().to_string()))
                }
                Some(b) => (CellStatus::DryRunOnly, Some(b.id().to_string())),
            };
            MatrixCell {
                action: id,
                group: "settings",
                label,
                status,
                path,
                paths_tried: tried,
            }
        })
        .collect()
}

pub fn not_found(ip: IpAddr, error: Option<String>) -> MatrixRow {
    MatrixRow {
        ip: ip.to_string(),
        found: false,
        class: None,
        make: None,
        model: None,
        firmware: None,
        driver: None,
        cells: Vec::new(),
        native: Vec::new(),
        error,
    }
}

pub fn matrix_row(
    engine: &SafetyEngine,
    ip: IpAddr,
    d: Arc<dyn MinerDriver>,
    miner: Option<&dyn asic_rs_core::traits::miner::Miner>,
) -> MatrixRow {
    let t = Target::new(ip, d.clone());
    let mut cells = control_cells(d.as_ref());
    cells.extend(settings_cells(engine, &t));
    MatrixRow {
        ip: ip.to_string(),
        found: true,
        class: Some(t.profile.class),
        make: Some(t.profile.make.clone()),
        model: Some(t.profile.model.clone()),
        firmware: Some(t.profile.firmware.clone()),
        driver: Some(d.kind().label()),
        cells,
        native: miner.map(native_flags).unwrap_or_default(),
        error: None,
    }
}
