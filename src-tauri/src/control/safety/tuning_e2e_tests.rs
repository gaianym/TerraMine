//! Tuning targets, presets and scaling (plan PR D) through the full safety
//! pipeline: batch runner → engine (path selection, limits, dry run,
//! snapshot, write, read-back, health watch, rollback, audit) → the real
//! asic-rs settings backend → per-firmware mocks that accept exactly the
//! target kinds each asic-rs 0.8.6 `set_tuning_config` does.
//!
//! - Mining mode: stock Antminer 2020+, Whatsminer, Sealminer (asic-rs);
//!   Bitaxe through the AxeOS frequency stand-in (Normal = chip stock
//!   frequency, Low = 90% of it). High is refused everywhere by the limits.
//! - Hashrate target: ePIC (asic-rs, `ChipTune` by default); Bitaxe via
//!   the frequency stand-in.
//! - Power target on ePIC: `set_tuning_config(Power)` (no `set_power_limit`).
//! - Presets: VNish, only presets the miner lists, wattage inside power_w.
//! - Scaling: ePIC.
//!
//! Everything else ends `no_path` with every path tried, before any write.

use std::net::IpAddr;
use std::time::Duration;

use asic_rs_core::data::miner::{MiningMode, TuningTarget};

use super::engine::{expected_ratio, WriteStatus, UNKNOWN_REDUCTION_RATIO};
use super::health::FailureCause;
use super::mock_native::NativeFirmware;
use super::power_e2e_tests::{ip_of, rig, run, AXE};
use super::settings::{Settings, TuningMode};

fn mode(m: TuningMode) -> Settings {
    Settings { tuning_mode: Some(m), ..Default::default() }
}
fn hashrate(h: u32) -> Settings {
    Settings { tuning_hashrate_ghs: Some(h), ..Default::default() }
}
fn preset(p: &str) -> Settings {
    Settings { tuning_preset: Some(p.into()), ..Default::default() }
}
fn scaling(step: u32, min: u32) -> Settings {
    Settings { scaling_step: Some(step), scaling_min: Some(min), ..Default::default() }
}

const MODE_CAPABLE: [NativeFirmware; 3] = [NativeFirmware::AntminerStock, NativeFirmware::WhatsminerV3, NativeFirmware::Sealminer2025];
const NO_MODE: [NativeFirmware; 6] = [
    NativeFirmware::Braiins2507,
    NativeFirmware::Vnish13,
    NativeFirmware::AvalonA,
    NativeFirmware::ProtoV1,
    NativeFirmware::AuradineV1,
    NativeFirmware::EpicV1,
];

fn natives() -> Vec<IpAddr> {
    NativeFirmware::ALL.into_iter().filter(|f| *f != NativeFirmware::BitaxeV214).map(ip_of).collect()
}

/// Dry run is the default and sends nothing; each class reports its path
/// or "not supported" with every path tried; the Bitaxe gets the
/// frequency stand-in, never an asic-rs tuning call.
#[tokio::test(flavor = "multi_thread")]
async fn mode_dry_run_by_default_per_class() {
    let req: crate::control::SettingsJobRequest =
        serde_json::from_value(serde_json::json!({"ips": ["192.0.2.100"], "change": {"tuning_mode": "low"}})).unwrap();
    assert!(req.dry_run, "tuning targets are a new write class: dry run by default");
    assert_eq!(req.change, mode(TuningMode::Low));

    let r = rig(false, true).await;
    let mut ips = natives();
    ips.push(IpAddr::from(AXE));
    let (o, alert) = run(&r, ips, mode(TuningMode::Low), true, None).await;
    for fw in MODE_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::DryRun, "{fw:?}: {x:?}");
        assert!(x.msg.as_deref().unwrap().contains("via asic-rs"), "{fw:?}: {x:?}");
    }
    for fw in NO_MODE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::NoPath, "{fw:?}: {x:?}");
        assert!(x.paths_tried[0].starts_with("asic-rs: skipped (asic-rs:"), "{fw:?}: {x:?}");
        assert!(x.msg.as_deref().unwrap().contains("tried:"), "{x:?}");
    }
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::DryRun, "{x:?}");
    let msg = x.msg.as_deref().unwrap();
    assert!(msg.contains("AxeOS has no mining modes; stand-in frequency 470 MHz (BM1370 stock 525 MHz"), "{msg}");
    assert!(msg.contains("frequency_mhz 525 → 470 via axeos-http"), "{msg}");
    assert!(x.paths_tried[0].contains("supports_tuning_config() = false"), "{x:?}");
    assert!(alert.is_none());
    for m in r.miners.values() {
        assert!(m.writes().is_empty(), "{:?}: dry run wrote", m.fw);
    }
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")), "nothing sent to the Bitaxe");
}

/// Live writes need a hardware-confirmed path; none is yet.
#[tokio::test(start_paused = true)]
async fn tuning_live_write_needs_a_confirmed_path() {
    let r = rig(false, false).await;
    let (o, _) = run(&r, MODE_CAPABLE.map(ip_of).to_vec(), mode(TuningMode::Low), false, None).await;
    for fw in MODE_CAPABLE {
        assert_eq!(o[&ip_of(fw)].status, WriteStatus::NoPath, "{fw:?}");
        assert!(r.m(fw).writes().is_empty());
    }
}

/// Low mode: one write, read back, verified with the hashrate floor
/// lowered for the intended reduction (70% of before would otherwise fail
/// the 80% floor and roll back).
#[tokio::test(start_paused = true)]
async fn low_mode_verified_with_snapshot_and_scaled_floor() {
    let r = rig(true, false).await;
    let (o, alert) = run(&r, MODE_CAPABLE.map(ip_of).to_vec(), mode(TuningMode::Low), false, None).await;
    for fw in MODE_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::Verified, "{fw:?}: {x:?}");
        assert_eq!(r.m(fw).writes(), ["set_tuning_config mode Low"], "{fw:?}");
        assert!(x.msg.as_deref().unwrap().contains("hashrate floor scaled to 50%"), "{x:?}");
        assert_eq!(x.after, Some(mode(TuningMode::Low)));
    }
    // Snapshot = whichever target ran before (stock Antminer: Normal mode;
    // Whatsminer / Sealminer: a power target).
    assert_eq!(o[&ip_of(NativeFirmware::AntminerStock)].before, Some(mode(TuningMode::Normal)));
    let w = &o[&ip_of(NativeFirmware::WhatsminerV3)];
    assert_eq!(w.before, Some(Settings { power_limit_w: Some(3000), ..Default::default() }));
    assert!(alert.is_none());
    let rec = r.audit.records();
    assert!(rec.iter().any(|a| a.phase == "attempt" && a.requested.as_ref().is_some_and(|v| v["tuning_mode"] == "low")));
}

/// A mode that makes the miner unhealthy is rolled back to the target
/// that ran before (here: Whatsminer's power target).
#[tokio::test(start_paused = true)]
async fn unhealthy_mode_rolls_back_to_previous_target() {
    let r = rig(true, false).await;
    let fw = NativeFirmware::WhatsminerV3;
    r.m(fw).set_unhealthy_mode(Some(MiningMode::Low));
    let (o, _) = run(&r, vec![ip_of(fw)], mode(TuningMode::Low), false, None).await;
    let x = &o[&ip_of(fw)];
    assert_eq!(x.status, WriteStatus::RolledBack, "{x:?}");
    assert_eq!(x.cause, Some(FailureCause::HashrateDrop));
    assert_eq!(r.m(fw).writes(), ["set_tuning_config mode Low", "set_power_limit 3000"]);
    assert!(matches!(r.m(fw).target().unwrap().target, TuningTarget::Power(_)));
}

/// High (overclock) is refused by the limits on every class, before any
/// write; Low on a non-A2 Sealminer is refused by the path.
///
/// Real time (multi_thread), not a paused clock: the Bitaxe here is the
/// real-socket AxeOS mock, and a paused clock auto-advances past the 2 s
/// resolve timeout while the socket I/O is still in flight (macOS CI
/// flake: "miner not found"). `paused_clock_tests_use_no_socket_mocks`
/// guards this.
#[tokio::test(flavor = "multi_thread")]
async fn high_mode_refused_everywhere() {
    let r = rig(true, true).await;
    let mut ips: Vec<IpAddr> = MODE_CAPABLE.map(ip_of).to_vec();
    ips.push(IpAddr::from(AXE));
    let (o, _) = run(&r, ips, mode(TuningMode::High), false, None).await;
    for fw in MODE_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::Refused, "{fw:?}: {x:?}");
        assert!(x.msg.as_deref().unwrap().contains("tuning_mode high is not allowed"), "{x:?}");
        assert!(r.m(fw).writes().is_empty());
    }
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::NoPath, "{x:?}");
    assert!(x.paths_tried.iter().any(|p| p.contains("AxeOS has no High mode")), "{x:?}");
}

#[test]
fn sealminer_low_mode_only_on_a2() {
    use super::asic::target_kinds;
    use super::profile::DeviceProfile;
    let a2 = DeviceProfile::classify("SealMiner", "A2", "SealMiner v2025", None);
    let a1 = DeviceProfile::classify("SealMiner", "A1", "SealMiner v2025", None);
    assert!(target_kinds(&a2).modes.contains(&TuningMode::Low));
    assert!(!target_kinds(&a1).modes.contains(&TuningMode::Low));
    assert!(target_kinds(&a1).modes.contains(&TuningMode::Normal));
}

/// ePIC hashrate target: default ChipTune algorithm, snapshot of the
/// running PowerTune target, rollback restores it with its algorithm.
#[tokio::test(start_paused = true)]
async fn epic_hashrate_target_and_rollback_restores_power_tune() {
    let r = rig(true, false).await;
    let fw = NativeFirmware::EpicV1;
    let (o, _) = run(&r, vec![ip_of(fw)], hashrate(90_000), false, None).await;
    let x = &o[&ip_of(fw)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert_eq!(r.m(fw).writes(), ["set_tuning_config hashrate 90000 GH/s (ChipTune)"]);
    assert_eq!(
        x.before,
        Some(Settings { power_limit_w: Some(3000), tuning_algorithm: Some("PowerTune".into()), ..Default::default() })
    );
    assert!(x.msg.as_deref().unwrap().contains("ChipTune algorithm"), "{x:?}");

    let r = rig(true, false).await;
    r.m(fw).set_ignore_writes(true);
    let (o, _) = run(&r, vec![ip_of(fw)], hashrate(90_000), false, None).await;
    let x = &o[&ip_of(fw)];
    assert_eq!(x.status, WriteStatus::RolledBack, "{x:?}");
    assert_eq!(x.cause, Some(FailureCause::ReadBackMismatch));
    assert_eq!(r.m(fw).writes(), ["set_tuning_config hashrate 90000 GH/s (ChipTune)", "set_tuning_config power 3000 W (PowerTune)"]);
}

/// Hashrate targets elsewhere: refused with paths tried; out of range refused.
#[tokio::test(start_paused = true)]
async fn hashrate_target_paths_and_limits() {
    let r = rig(true, false).await;
    let (o, _) = run(&r, natives(), hashrate(90_000), true, None).await;
    for fw in NativeFirmware::ALL.into_iter().filter(|f| !matches!(f, NativeFirmware::EpicV1 | NativeFirmware::BitaxeV214)) {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::NoPath, "{fw:?}: {x:?}");
        assert!(x.paths_tried[0].contains("hashrate target"), "{x:?}");
    }
    let (o, _) = run(&r, vec![ip_of(NativeFirmware::EpicV1)], hashrate(200_000), false, None).await;
    let x = &o[&ip_of(NativeFirmware::EpicV1)];
    assert_eq!(x.status, WriteStatus::Refused);
    assert!(x.msg.as_deref().unwrap().contains("tuning_hashrate_ghs 200000 outside 50000..=110000"), "{x:?}");
    assert!(r.m(NativeFirmware::EpicV1).writes().is_empty());
}

/// ePIC has no `set_power_limit`; a power target goes through
/// `set_tuning_config(Power)` with the safety steps.
#[tokio::test(start_paused = true)]
async fn epic_power_target_through_tuning_config() {
    let r = rig(true, false).await;
    let fw = NativeFirmware::EpicV1;
    let (o, _) = run(&r, vec![ip_of(fw)], Settings { power_limit_w: Some(2500), ..Default::default() }, false, None).await;
    let x = &o[&ip_of(fw)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert_eq!(r.m(fw).writes(), ["set_tuning_config power 2500 W"]);
    assert_eq!(r.m(fw).power(), 2500);
}

/// VNish presets: only listed presets, wattage inside power_w; elsewhere
/// not supported.
#[tokio::test(start_paused = true)]
async fn vnish_presets() {
    let r = rig(true, false).await;
    let v = NativeFirmware::Vnish13;
    let (o, _) = run(&r, vec![ip_of(v)], preset("2500"), false, None).await;
    let x = &o[&ip_of(v)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert_eq!(r.m(v).writes(), ["set_tuning_config preset 2500"]);
    assert_eq!(x.before, Some(Settings { power_limit_w: Some(3000), ..Default::default() }));
    assert!(x.msg.as_deref().unwrap().contains("hashrate floor scaled"), "{x:?}");

    let (o, _) = run(&r, vec![ip_of(v)], preset("2600"), false, None).await;
    let x = &o[&ip_of(v)];
    assert_eq!(x.status, WriteStatus::Refused, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("not one this miner offers (available: 2500, 3000, 3250)"), "{x:?}");

    let (o, _) = run(&r, vec![ip_of(v)], preset("5000"), false, None).await;
    assert!(o[&ip_of(v)].msg.as_deref().unwrap().contains("preset 5000 (5000 W) outside power_w 1000..=3250"));
    assert_eq!(r.m(v).writes().len(), 1, "refused presets write nothing");

    let (o, _) = run(&r, vec![ip_of(NativeFirmware::Braiins2507), ip_of(NativeFirmware::EpicV1)], preset("2500"), true, None).await;
    for x in o.values() {
        assert_eq!(x.status, WriteStatus::NoPath, "{x:?}");
        assert!(x.paths_tried[0].contains("supports_presets() = false"), "{x:?}");
    }
}

/// ePIC scaling: both fields, snapshot, verified, rollback on mismatch.
#[tokio::test(start_paused = true)]
async fn epic_scaling() {
    let r = rig(true, false).await;
    let e = NativeFirmware::EpicV1;
    let (o, _) = run(&r, vec![ip_of(e)], scaling(2, 60), false, None).await;
    let x = &o[&ip_of(e)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert_eq!(r.m(e).writes(), ["set_scaling_config step 2 min 60"]);
    assert_eq!(x.before, Some(scaling(5, 50)));
    assert_eq!(r.m(e).scaling(), (2, 60));

    let (o, _) = run(&r, vec![ip_of(e)], Settings { scaling_min: Some(60), ..Default::default() }, true, None).await;
    assert!(o[&ip_of(e)].paths_tried[0].contains("needs both scaling_step and scaling_min"));

    let (o, _) = run(&r, vec![ip_of(e)], scaling(20, 60), false, None).await;
    assert!(o[&ip_of(e)].msg.as_deref().unwrap().contains("scaling_step 20 outside 1..=10"));

    let (o, _) = run(&r, vec![ip_of(NativeFirmware::Vnish13), IpAddr::from(AXE)], scaling(2, 60), true, None).await;
    let v = &o[&ip_of(NativeFirmware::Vnish13)];
    assert_eq!(v.status, WriteStatus::NoPath);
    assert!(v.paths_tried[0].contains("supports_scaling_config() = false"), "{v:?}");
}

/// One target per miner: two kinds at once are refused before any write.
#[tokio::test(start_paused = true)]
async fn one_target_kind_at_a_time() {
    let r = rig(true, false).await;
    let both = Settings { tuning_mode: Some(TuningMode::Low), power_limit_w: Some(2500), ..Default::default() };
    let (o, _) = run(&r, vec![ip_of(NativeFirmware::WhatsminerV3)], both, false, None).await;
    let x = &o[&ip_of(NativeFirmware::WhatsminerV3)];
    assert_eq!(x.status, WriteStatus::NoPath, "{x:?}");
    assert!(x.paths_tried[0].contains("runs one tuning target"), "{x:?}");
    assert!(r.m(NativeFirmware::WhatsminerV3).writes().is_empty());
}

/// Bitaxe hashrate target: AxeOS frequency stand-in (reuses the PR H
/// path), live on the mock, read back and verified; only AxeOS HTTP calls.
#[tokio::test(flavor = "multi_thread")]
async fn bitaxe_hashrate_target_uses_frequency_stand_in() {
    let r = rig(true, true).await;
    let (o, _) = run(&r, vec![IpAddr::from(AXE)], hashrate(1000), true, None).await;
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::DryRun, "{x:?}");
    let msg = x.msg.as_deref().unwrap();
    assert!(msg.contains("AxeOS has no hashrate target; stand-in frequency 525 → 440 MHz"), "{msg}");
    assert_eq!(x.attempted, Some(Settings { frequency_mhz: Some(440), ..Default::default() }));
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")));
    // A raise via a hashrate target is held like any frequency raise.
    let (o, _) = run(&r, vec![IpAddr::from(AXE)], hashrate(1300), false, None).await;
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::Refused, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("needs an explicit OK (allow_frequency_raise)"), "{x:?}");
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")));
    // Presets / scaling on a Bitaxe: not supported, with every path's reason.
    let (o, _) = run(&r, vec![IpAddr::from(AXE)], preset("2500"), true, None).await;
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::NoPath, "{x:?}");
    assert!(x.paths_tried.iter().any(|p| p.contains("AxeOS has no tuning presets")), "{x:?}");
}

/// Regression (permanent): an intended reduction no longer trips the
/// hashrate-drop rollback. Before PR D the floor was 80% of the
/// pre-change hashrate whatever the change, so lowering power by a third
/// on a healthy miner was rolled back.
#[tokio::test(start_paused = true)]
async fn intended_power_reduction_is_not_rolled_back() {
    let r = rig(true, false).await;
    let b = NativeFirmware::Braiins2507;
    let (o, _) = run(&r, vec![ip_of(b)], Settings { power_limit_w: Some(2000), ..Default::default() }, false, None).await;
    let x = &o[&ip_of(b)];
    assert_eq!(x.status, WriteStatus::Verified, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("hashrate floor scaled to 67%"), "{x:?}");
    // A real failure under the reduced floor still rolls back.
    let r = rig(true, false).await;
    r.m(b).set_unhealthy_below(Some(2500));
    let (o, _) = run(&r, vec![ip_of(b)], Settings { power_limit_w: Some(2000), ..Default::default() }, false, None).await;
    assert_eq!(o[&ip_of(b)].status, WriteStatus::RolledBack);
}

#[test]
fn expected_ratio_rules() {
    let p = |w| Settings { power_limit_w: Some(w), ..Default::default() };
    assert_eq!(expected_ratio(&p(3000), &p(1500), Some(100.0)), Some(0.5));
    assert_eq!(expected_ratio(&p(3000), &p(3250), Some(100.0)), None, "increases never raise the floor");
    assert_eq!(expected_ratio(&mode(TuningMode::Normal), &mode(TuningMode::Low), None), Some(UNKNOWN_REDUCTION_RATIO));
    assert_eq!(expected_ratio(&mode(TuningMode::Low), &mode(TuningMode::Normal), None), None);
    assert_eq!(expected_ratio(&p(3000), &hashrate(80), Some(100.0)), Some(0.8));
    let f = |m| Settings { frequency_mhz: Some(m), ..Default::default() };
    assert_eq!(expected_ratio(&f(500), &f(400), None), Some(0.8));
    assert_eq!(expected_ratio(&scaling(5, 50), &scaling(2, 60), Some(1.0)), None);
}

/// Support rows: modes / hashrate / presets / scaling per class.
#[tokio::test(flavor = "multi_thread")]
async fn support_rows_for_tuning_targets() {
    use crate::control::cache::Resolved;
    let r = rig(false, true).await;
    let row = |ip: IpAddr| {
        let r = &r;
        async move {
            let d = match r.resolver.resolve(ip, Duration::from_secs(2)).await {
                Resolved::Ready { driver, .. } => Some(driver),
                _ => None,
            };
            crate::control::settings_support_row(&r.engine, ip, d).await
        }
    };
    let v = row(ip_of(NativeFirmware::Vnish13)).await;
    assert_eq!(v.preset.path.as_deref(), Some("asic-rs"));
    assert_eq!(v.presets, ["2500", "3000", "3250"]);
    assert!(v.mode.path.is_none() && v.scaling.path.is_none());
    let e = row(ip_of(NativeFirmware::EpicV1)).await;
    assert_eq!((e.hashrate.path.as_deref(), e.scaling.path.as_deref()), (Some("asic-rs"), Some("asic-rs")));
    assert_eq!((e.tuning_hashrate_ghs, e.scaling_step, e.scaling_min), (Some([50000, 110000]), Some([1, 10]), Some([50, 100])));
    let a = row(ip_of(NativeFirmware::AntminerStock)).await;
    assert_eq!(a.mode.path.as_deref(), Some("asic-rs"));
    assert_eq!(a.tuning_modes, [TuningMode::Low, TuningMode::Normal]);
    let b = row(IpAddr::from(AXE)).await;
    assert_eq!((b.mode.path.as_deref(), b.hashrate.path.as_deref()), (Some("axeos-http"), Some("axeos-http")));
    assert!(b.preset.path.is_none() && b.presets.is_empty());
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json["tuning_modes"], serde_json::json!(["low", "normal"]));
}
