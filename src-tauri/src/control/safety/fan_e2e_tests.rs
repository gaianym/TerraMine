//! Fan + temperature fleetwide (plan PR E) through the full safety
//! pipeline: batch runner → resolver → engine → the real asic-rs settings
//! backend (per-firmware mocks) or the AxeOS override (mock AxeOS) → audit.
//!
//! asic-rs 0.8.6 `supports_fan_config()`: stock Antminer, ePIC, Proto.
//! Stock Antminer auto has no target temperature (only `bitmain-fan-ctrl`).
//! `supports_temperature_config()` (VNish) is read only: no firmware
//! implements `set_temperature_config`, so threshold writes end `no_path`.
//! Bitaxe fan goes through the AxeOS path from PR H. Stock S9 has no safe
//! partial write route and reports why.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use asic_rs_core::config::fan::FanConfig;
use tokio_util::sync::CancellationToken;

use super::asic::{asic_accepts, fan_config_for, settings_from_configs, NativeSettingsFlags};
use super::audit::MemoryAudit;
use super::batch::{run_settings_job, BatchOptions, JobAlert};
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, WriteOutcome, WriteStatus};
use super::health::{FailureCause, HealthPolicy};
use super::mock_native::{MockNative, MockNativeDriver, MockNativeFactory, NativeFirmware};
use super::policy::Confirmation;
use super::settings::Settings;
use super::snapshot::SnapshotStore;
use crate::control::cache::{DeviceCache, Resolver};
use crate::control::driver::{DriverFactory, MinerDriver};
use crate::control::mock_miner::{MockKind, MockMiner, MockMinerFactory};

const AXE: [u8; 4] = [192, 0, 2, 90];
const S9: [u8; 4] = [192, 0, 2, 91];

const FAN_CAPABLE: [NativeFirmware; 3] = [NativeFirmware::AntminerStock, NativeFirmware::EpicV1, NativeFirmware::ProtoV1];
const NO_FAN: [NativeFirmware; 6] = [
    NativeFirmware::Braiins2507,
    NativeFirmware::Vnish13,
    NativeFirmware::WhatsminerV3,
    NativeFirmware::AvalonA,
    NativeFirmware::Sealminer2025,
    NativeFirmware::AuradineV1,
];

fn ip_of(fw: NativeFirmware) -> IpAddr {
    let i = NativeFirmware::ALL.iter().position(|f| *f == fw).unwrap() as u8;
    IpAddr::from([192, 0, 2, 100 + i])
}

struct Rig {
    miners: HashMap<NativeFirmware, Arc<MockNative>>,
    resolver: Arc<Resolver>,
    audit: Arc<MemoryAudit>,
    engine: Arc<SafetyEngine>,
    axe: Option<MockMiner>,
    s9: Option<MockMiner>,
}

impl Rig {
    fn m(&self, fw: NativeFirmware) -> &Arc<MockNative> {
        &self.miners[&fw]
    }
}

fn opts(allow_unconfirmed: bool) -> SafetyOptions {
    SafetyOptions {
        op_timeout: Duration::from_secs(5),
        readback_timeout: Duration::from_secs(10),
        health: HealthPolicy {
            window: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            settle: Duration::from_secs(20),
            min_hashrate_ratio: 0.8,
            max_temp_c: None,
            max_unreachable_polls: 2,
        },
        rollback_timeout: Duration::from_secs(30),
        allow_unconfirmed_paths: allow_unconfirmed,
    }
}

/// `with_http`: also a mock AxeOS 2.15 Bitaxe and a stock S9 (dry runs only).
async fn rig(allow_unconfirmed: bool, with_http: bool) -> Rig {
    let miners: HashMap<NativeFirmware, Arc<MockNative>> =
        NativeFirmware::ALL.into_iter().filter(|f| *f != NativeFirmware::BitaxeV214).map(|f| (f, MockNative::new(f))).collect();
    let (axe, s9, other) = if with_http {
        let axe = MockMiner::spawn(MockKind::AxeOs215, Duration::from_millis(200)).await.unwrap();
        let s9 = MockMiner::spawn(MockKind::S9Stock { miner_mode: false }, Duration::from_millis(200)).await.unwrap();
        let mut f = MockMinerFactory::new(Duration::from_secs(2));
        f.add(IpAddr::from(AXE), &axe);
        f.add(IpAddr::from(S9), &s9);
        (Some(axe), Some(s9), Some(Arc::new(f) as Arc<dyn DriverFactory>))
    } else {
        (None, None, None)
    };
    let factory = MockNativeFactory { miners: miners.iter().map(|(f, m)| (ip_of(*f), m.clone())).collect(), other };
    let resolver = Arc::new(Resolver::new(Arc::new(factory), Arc::new(DeviceCache::default())));
    let audit = Arc::new(MemoryAudit::default());
    let engine = Arc::new(SafetyEngine::new(audit.clone(), Arc::new(SnapshotStore::in_memory(64)), opts(allow_unconfirmed)));
    Rig { miners, resolver, audit, engine, axe, s9 }
}

fn batch(canary: Option<usize>) -> BatchOptions {
    BatchOptions { concurrency: 4, stagger: Duration::from_millis(10), canary_size: canary, resolve_timeout: Duration::from_secs(2), ..Default::default() }
}

async fn run_with(
    r: &Rig,
    ips: Vec<IpAddr>,
    change: Settings,
    dry_run: bool,
    canary: Option<usize>,
    confirm: Confirmation,
) -> (HashMap<IpAddr, WriteOutcome>, Option<JobAlert>) {
    let req = SafeWriteRequest { change, dry_run, confirm };
    let (out, alert) =
        run_settings_job("fan-run", ips, req, r.resolver.clone(), r.engine.clone(), batch(canary), CancellationToken::new(), Arc::new(|_| {})).await;
    (out.into_iter().map(|o| (o.ip.parse().unwrap(), o)).collect(), alert)
}

async fn run(r: &Rig, ips: Vec<IpAddr>, change: Settings, dry_run: bool, canary: Option<usize>) -> (HashMap<IpAddr, WriteOutcome>, Option<JobAlert>) {
    run_with(r, ips, change, dry_run, canary, Confirmation::default()).await
}

fn manual(p: u32) -> Settings {
    Settings { auto_fan: Some(false), fan_pct: Some(p), ..Default::default() }
}

fn auto(t: Option<u32>) -> Settings {
    Settings { auto_fan: Some(true), temp_target_c: t, ..Default::default() }
}

fn ips(fws: &[NativeFirmware]) -> Vec<IpAddr> {
    fws.iter().map(|f| ip_of(*f)).collect()
}

fn all_native() -> Vec<IpAddr> {
    FAN_CAPABLE.iter().chain(NO_FAN.iter()).map(|f| ip_of(*f)).collect()
}

/// Default options: a dry run per miner naming the path, or not supported
/// with every path tried; the Bitaxe goes through AxeOS; the S9 says why
/// it has no route. Nothing is written anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn fan_dry_run_fleetwide_shows_path_or_not_supported() {
    let r = rig(false, true).await;
    let mut all = all_native();
    all.extend([IpAddr::from(AXE), IpAddr::from(S9)]);
    let (o, alert) = run(&r, all, manual(70), true, None).await;
    for fw in FAN_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::DryRun, "{fw:?}: {x:?}");
        let msg = x.msg.as_deref().unwrap();
        assert!(msg.contains("via asic-rs") && msg.contains("70"), "{fw:?}: {msg}");
        assert_eq!(x.paths_tried, ["asic-rs: selected for dry run only (not yet confirmed on real hardware)"]);
    }
    for fw in NO_FAN {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::NoPath, "{fw:?}: {x:?}");
        assert!(x.paths_tried[0].contains("supports_fan_config() = false"), "{x:?}");
    }
    let b = &o[&IpAddr::from(AXE)];
    assert_eq!(b.status, WriteStatus::DryRun, "{b:?}");
    assert!(b.paths_tried[0].contains("supports_fan_config() = false"), "{b:?}");
    // Fan on AxeOS 2.15.x is hardware-confirmed.
    assert_eq!(b.paths_tried[1], "axeos-http: selected", "{b:?}");
    assert!(b.msg.as_deref().unwrap().contains("via axeos-http"), "{b:?}");
    let s = &o[&IpAddr::from(S9)];
    assert_eq!(s.status, WriteStatus::NoPath, "{s:?}");
    assert!(s.paths_tried[0].starts_with("s9-cgi: skipped (stock S9 firmware: set_miner_conf.cgi replaces the whole miner config"), "{s:?}");
    assert!(alert.is_none());
    for m in r.miners.values() {
        assert!(m.writes().is_empty(), "{:?}: dry run wrote", m.fw);
    }
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")));
    assert!(!r.s9.as_ref().unwrap().requests().iter().any(|q| q.contains("set_miner_conf")), "nothing sent to the S9");
}

/// Auto with a target temperature where the firmware honours it (ePIC,
/// Proto); stock Antminer only takes plain auto.
#[tokio::test(start_paused = true)]
async fn auto_fan_target_only_where_the_firmware_honours_it() {
    let r = rig(true, false).await;
    let (o, _) = run(&r, ips(&FAN_CAPABLE), auto(Some(68)), false, None).await;
    for fw in [NativeFirmware::EpicV1, NativeFirmware::ProtoV1] {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::Verified, "{fw:?}: {x:?}");
        assert_eq!(x.after.as_ref().unwrap().temp_target_c, Some(68));
    }
    assert!(matches!(r.m(NativeFirmware::EpicV1).fan(), FanConfig::Auto { target_temp, idle_speed: Some(25) } if target_temp == 68.0));
    let a = &o[&ip_of(NativeFirmware::AntminerStock)];
    assert_eq!(a.status, WriteStatus::NoPath, "{a:?}");
    assert!(a.paths_tried[0].contains("stock Antminer auto fan has no target temperature"), "{a:?}");
    assert!(r.m(NativeFirmware::AntminerStock).writes().is_empty());

    // Antminer: manual, then plain auto back.
    let am = ip_of(NativeFirmware::AntminerStock);
    let (o, _) = run(&r, vec![am], manual(80), false, None).await;
    assert_eq!(o[&am].status, WriteStatus::Verified, "{:?}", o[&am]);
    let (o, _) = run(&r, vec![am], auto(None), false, None).await;
    assert_eq!(o[&am].status, WriteStatus::Verified, "{:?}", o[&am]);
    assert_eq!(o[&am].before, Some(manual(80)), "fan group snapshot: mode + manual %");
    assert_eq!(r.m(NativeFirmware::AntminerStock).writes(), ["set_fan_config manual 80", "set_fan_config auto 0 idle Some(80)"]);

    // ePIC / Proto need a target when switching to auto.
    let (o, _) = run(&r, vec![ip_of(NativeFirmware::ProtoV1)], auto(None), false, None).await;
    assert!(o[&ip_of(NativeFirmware::ProtoV1)].paths_tried[0].contains("auto fan needs a target temperature"));
}

/// Mock-only opt-in: manual fan on every fan-capable firmware, one write,
/// whole fan state snapshotted (mode + target) and audited.
#[tokio::test(start_paused = true)]
async fn manual_fan_fleetwide_verified_with_fan_group_snapshot() {
    let r = rig(true, false).await;
    let (o, alert) = run(&r, all_native(), manual(70), false, Some(1)).await;
    for fw in FAN_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::Verified, "{fw:?}: {x:?}");
        assert_eq!(r.m(fw).writes(), ["set_fan_config manual 70"], "{fw:?}: exactly one write");
        assert_eq!(r.m(fw).fan(), FanConfig::Manual { fan_speed: 70 });
        let snap = r.engine.snapshots.get(x.snapshot_id.as_deref().unwrap()).unwrap();
        assert_eq!(snap.settings.auto_fan, Some(true), "{fw:?}: the mode is snapshotted");
    }
    assert_eq!(o[&ip_of(NativeFirmware::EpicV1)].before, Some(auto(Some(70))));
    assert_eq!(o[&ip_of(NativeFirmware::AntminerStock)].before, Some(auto(None)), "stock auto reports no target");
    for fw in NO_FAN {
        assert_eq!(o[&ip_of(fw)].status, WriteStatus::NoPath);
        assert!(r.m(fw).writes().is_empty());
    }
    assert!(alert.is_none());
    let recs = r.audit.records();
    assert_eq!(recs.iter().filter(|x| x.phase == "attempt").count(), FAN_CAPABLE.len());
}

/// Permanent: a rollback to auto keeps the firmware's idle speed (ePIC
/// would otherwise reset it to asic-rs's default of 20) and restores the
/// target temperature. Overheat after a low manual speed triggers it.
#[tokio::test(start_paused = true)]
async fn overheat_rolls_back_to_auto_with_target_and_idle_speed() {
    let r = rig(true, false).await;
    let fw = NativeFirmware::EpicV1;
    r.m(fw).set_overheat_below_fan(Some(50));
    let (o, alert) = run(&r, vec![ip_of(fw)], manual(40), false, None).await;
    let x = &o[&ip_of(fw)];
    assert_eq!(x.status, WriteStatus::RolledBack, "{x:?}");
    assert_eq!(x.cause, Some(FailureCause::Overheat));
    assert_eq!(r.m(fw).fan(), FanConfig::Auto { target_temp: 70.0, idle_speed: Some(25) });
    assert_eq!(r.m(fw).writes(), ["set_fan_config manual 40", "set_fan_config auto 70 idle Some(25)"]);
    assert_eq!(x.after, Some(auto(Some(70))));
    assert_eq!(alert.unwrap().groups[0].cause, FailureCause::Overheat);
}

/// Health watch → rollback → aggregated alert; the canary halts the rest.
#[tokio::test(start_paused = true)]
async fn fan_canary_halts_after_overheat_rollback() {
    let r = rig(true, false).await;
    for fw in FAN_CAPABLE {
        r.m(fw).set_overheat_below_fan(Some(90));
    }
    let (o, alert) = run(&r, ips(&FAN_CAPABLE), manual(60), false, Some(1)).await;
    let first = &o[&ip_of(FAN_CAPABLE[0])];
    assert_eq!(first.status, WriteStatus::RolledBack, "{first:?}");
    for fw in &FAN_CAPABLE[1..] {
        let x = &o[&ip_of(*fw)];
        assert_eq!((x.status, x.skip_reason.as_deref()), (WriteStatus::Skipped, Some("halted")), "{x:?}");
        assert!(r.m(*fw).writes().is_empty());
    }
    let a = alert.expect("alert");
    assert!(a.headline.starts_with("1 of 3 rolled back, 2 halted"), "{}", a.headline);
    assert!(a.retry.dry_run);
}

/// Manual fan below 30% needs `force_low_fan` (it matters on Bitaxe, whose
/// placeholder range starts at 20%): a note in the dry run, a refusal live
/// with nothing sent. Limits refuse first where the range starts at 30%.
#[tokio::test(flavor = "multi_thread")]
async fn low_manual_fan_needs_force_and_limits() {
    let r = rig(true, true).await;
    let axe = IpAddr::from(AXE);
    let (o, _) = run(&r, vec![axe], manual(25), true, None).await;
    assert_eq!(o[&axe].status, WriteStatus::DryRun, "{:?}", o[&axe]);
    assert!(o[&axe].msg.as_deref().unwrap().contains("manual fan 25% below 30% is held: needs force_low_fan"), "{:?}", o[&axe]);
    let (o, _) = run(&r, vec![axe], manual(25), false, None).await;
    assert_eq!(o[&axe].status, WriteStatus::Refused, "{:?}", o[&axe]);
    assert!(o[&axe].msg.as_deref().unwrap().contains("needs force_low_fan"));
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")), "nothing sent");
    let force = Confirmation { force_low_fan: true, ..Default::default() };
    let (o, _) = run_with(&r, vec![axe], manual(25), true, None, force.clone()).await;
    assert!(!o[&axe].msg.as_deref().unwrap().contains("held"), "{:?}", o[&axe]);

    let epic = ip_of(NativeFirmware::EpicV1);
    let (o, _) = run_with(&r, vec![epic], manual(20), false, None, force).await;
    assert_eq!(o[&epic].status, WriteStatus::Refused);
    assert!(o[&epic].msg.as_deref().unwrap().contains("fan_pct 20 outside 30..=100"), "{:?}", o[&epic]);
    assert!(r.m(NativeFirmware::EpicV1).writes().is_empty());
    let (o, _) = run(&r, vec![epic], auto(Some(80)), false, None).await;
    assert!(o[&epic].msg.as_deref().unwrap().contains("temp_target_c 80 outside 55..=75"), "{:?}", o[&epic]);
}

/// Thermal thresholds: read where asic-rs supports it (VNish), never
/// written (no 0.8.6 firmware implements `set_temperature_config`).
#[tokio::test(flavor = "multi_thread")]
async fn temperature_thresholds_are_read_only_everywhere() {
    let r = rig(true, true).await;
    let change = Settings { temp_hot_c: Some(85), ..Default::default() };
    let mut all = all_native();
    all.push(IpAddr::from(AXE));
    let (o, _) = run(&r, all, change, false, None).await;
    let v = &o[&ip_of(NativeFirmware::Vnish13)];
    assert_eq!(v.status, WriteStatus::NoPath, "{v:?}");
    assert!(v.paths_tried[0].contains("supports_temperature_config() = true; no firmware implements set_temperature_config"), "{v:?}");
    let b = &o[&ip_of(NativeFirmware::Braiins2507)];
    assert!(b.paths_tried[0].contains("supports_temperature_config() = false"), "{b:?}");
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::NoPath);
    assert!(x.paths_tried[1].contains("AxeOS has no thermal threshold setting"), "{x:?}");
    for m in r.miners.values() {
        assert!(m.writes().is_empty());
    }
    let d = MockNativeDriver::new(ip_of(NativeFirmware::Vnish13), r.m(NativeFirmware::Vnish13).clone());
    let s = d.settings_backends()[0].read().await.unwrap();
    assert_eq!((s.temp_hot_c, s.temp_danger_c), (Some(80), Some(90)));
}

#[test]
fn fan_fields_map_to_one_fan_config() {
    let f = NativeSettingsFlags { fan: true, auto_target: true, ..Default::default() };
    let stock = NativeSettingsFlags { fan: true, ..Default::default() };
    assert!(asic_accepts(&manual(70), f).is_ok());
    assert!(asic_accepts(&auto(Some(60)), f).is_ok());
    assert!(asic_accepts(&auto(None), stock).is_ok());
    let both = Settings { auto_fan: Some(true), fan_pct: Some(50), temp_target_c: Some(60), ..Default::default() };
    assert!(asic_accepts(&both, f).unwrap_err().contains("not both"));
    let manual_target = Settings { auto_fan: Some(false), fan_pct: Some(50), temp_target_c: Some(60), ..Default::default() };
    assert!(asic_accepts(&manual_target, f).unwrap_err().contains("only applies in auto"));
    let bare = Settings { temp_target_c: Some(60), ..Default::default() };
    assert!(asic_accepts(&bare, f).unwrap_err().contains("needs auto_fan: true"));
    assert!(asic_accepts(&Settings { auto_fan: Some(false), ..Default::default() }, f).unwrap_err().contains("needs fan_pct"));
    assert!(asic_accepts(&manual(70), NativeSettingsFlags::default()).unwrap_err().contains("supports_fan_config() = false"));

    assert_eq!(fan_config_for(&manual(70), Some(9)), Some(FanConfig::Manual { fan_speed: 70 }));
    assert_eq!(fan_config_for(&auto(Some(65)), Some(25)), Some(FanConfig::Auto { target_temp: 65.0, idle_speed: Some(25) }));
    assert_eq!(fan_config_for(&Settings { fan_pct: Some(40), ..Default::default() }, None), Some(FanConfig::Manual { fan_speed: 40 }));
    assert_eq!(fan_config_for(&Settings::default(), None), None);

    let s = settings_from_configs(None, Some(&FanConfig::Auto { target_temp: 70.0, idle_speed: Some(20) }));
    assert_eq!(s, auto(Some(70)));
    let s = settings_from_configs(None, Some(&FanConfig::Auto { target_temp: 0.0, idle_speed: Some(100) }));
    assert_eq!(s, auto(None), "stock Antminer auto: no target");
}

#[test]
fn every_fan_capable_mock_has_fan_limits() {
    let t = super::limits::LimitsTable::builtin();
    for fw in FAN_CAPABLE {
        let (make, model, f) = fw.identity();
        let p = super::DeviceProfile::classify(make, model, f, None);
        assert!(t.lookup(&p).and_then(|e| e.fan_pct).is_some(), "{fw:?} ({p:?}) has no fan_pct range");
    }
}
