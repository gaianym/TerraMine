//! Power limit + tuning percent through the full safety pipeline:
//! batch runner → resolver → engine (path selection, limits, dry run,
//! snapshot, write, read-back, health watch, rollback, audit) → the real
//! asic-rs settings backend → per-firmware mocks.
//!
//! Firmwares with `supports_set_power_limit()` (Braiins, VNish, Whatsminer,
//! Avalon, Proto, Sealminer, Auradine) take the write; stock Antminer,
//! ePIC and Bitaxe have no power path and end `no_path` with every path
//! tried, before anything is sent.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::audit::MemoryAudit;
use super::batch::{run_settings_job, BatchOptions, JobAlert};
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, WriteOutcome, WriteStatus};
use super::health::{FailureCause, HealthPolicy};
use super::mock_native::{MockNative, MockNativeFactory, NativeFirmware};
use super::policy::Confirmation;
use super::settings::Settings;
use super::snapshot::SnapshotStore;
use crate::control::cache::{DeviceCache, Resolver};
use crate::control::driver::DriverFactory;
use crate::control::mock_miner::{MockKind, MockMiner, MockMinerFactory};

pub(super) const AXE: [u8; 4] = [192, 0, 2, 90];

pub(super) fn ip_of(fw: NativeFirmware) -> IpAddr {
    let i = NativeFirmware::ALL.iter().position(|f| *f == fw).unwrap() as u8;
    IpAddr::from([192, 0, 2, 100 + i])
}

const POWER_CAPABLE: [NativeFirmware; 7] = [
    NativeFirmware::Braiins2507,
    NativeFirmware::Vnish13,
    NativeFirmware::WhatsminerV3,
    NativeFirmware::AvalonA,
    NativeFirmware::ProtoV1,
    NativeFirmware::Sealminer2025,
    NativeFirmware::AuradineV1,
];
const NO_POWER: [NativeFirmware; 2] = [NativeFirmware::AntminerStock, NativeFirmware::EpicV1];

pub(super) struct Rig {
    pub(super) miners: HashMap<NativeFirmware, Arc<MockNative>>,
    pub(super) resolver: Arc<Resolver>,
    pub(super) audit: Arc<MemoryAudit>,
    pub(super) engine: Arc<SafetyEngine>,
    pub(super) axe: Option<MockMiner>,
}

impl Rig {
    pub(super) fn m(&self, fw: NativeFirmware) -> &Arc<MockNative> {
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

pub(super) async fn rig(allow_unconfirmed: bool, with_bitaxe: bool) -> Rig {
    let miners: HashMap<NativeFirmware, Arc<MockNative>> =
        NativeFirmware::ALL.into_iter().filter(|f| *f != NativeFirmware::BitaxeV214).map(|f| (f, MockNative::new(f))).collect();
    let (axe, other) = if with_bitaxe {
        let axe = MockMiner::spawn(MockKind::AxeOs215, Duration::from_millis(200)).await.unwrap();
        let mut f = MockMinerFactory::new(Duration::from_secs(2));
        f.add(IpAddr::from(AXE), &axe);
        (Some(axe), Some(Arc::new(f) as Arc<dyn DriverFactory>))
    } else {
        (None, None)
    };
    let factory = MockNativeFactory { miners: miners.iter().map(|(f, m)| (ip_of(*f), m.clone())).collect(), other };
    let resolver = Arc::new(Resolver::new(Arc::new(factory), Arc::new(DeviceCache::default())));
    let audit = Arc::new(MemoryAudit::default());
    let engine = Arc::new(SafetyEngine::new(audit.clone(), Arc::new(SnapshotStore::in_memory(64)), opts(allow_unconfirmed)));
    Rig { miners, resolver, audit, engine, axe }
}

pub(super) fn batch(canary: Option<usize>) -> BatchOptions {
    BatchOptions { concurrency: 4, stagger: Duration::from_millis(10), canary_size: canary, resolve_timeout: Duration::from_secs(2), ..Default::default() }
}

pub(super) async fn run(r: &Rig, ips: Vec<IpAddr>, change: Settings, dry_run: bool, canary: Option<usize>) -> (HashMap<IpAddr, WriteOutcome>, Option<JobAlert>) {
    let req = SafeWriteRequest { change, dry_run, confirm: Confirmation::default() };
    let (out, alert) =
        run_settings_job("power-run", ips, req, r.resolver.clone(), r.engine.clone(), batch(canary), CancellationToken::new(), Arc::new(|_| {})).await;
    (out.into_iter().map(|o| (o.ip.parse().unwrap(), o)).collect(), alert)
}

fn power(w: u32) -> Settings {
    Settings { power_limit_w: Some(w), ..Default::default() }
}

fn pct(p: u32) -> Settings {
    Settings { tuning_pct: Some(p), ..Default::default() }
}

fn all_native() -> Vec<IpAddr> {
    POWER_CAPABLE.iter().chain(NO_POWER.iter()).map(|f| ip_of(*f)).collect()
}

/// App defaults (no backend hardware-confirmed): the request is a dry run
/// unless turned off, the dry run names the path, nothing is written.
#[tokio::test(flavor = "multi_thread")]
async fn power_dry_run_by_default_shows_path_or_not_supported_per_class() {
    let req: crate::control::SettingsJobRequest =
        serde_json::from_value(serde_json::json!({"ips": ["192.0.2.100"], "change": {"power_limit_w": 2500}})).unwrap();
    assert!(req.dry_run, "power is a new write class: dry run by default");

    let r = rig(false, true).await;
    let mut ips = all_native();
    ips.push(IpAddr::from(AXE));
    let (o, alert) = run(&r, ips, power(2500), true, None).await;
    for fw in POWER_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::DryRun, "{fw:?}: {x:?}");
        let msg = x.msg.as_deref().unwrap();
        assert!(msg.contains("power_limit_w 3000 → 2500 via asic-rs"), "{fw:?}: {msg}");
        assert!(msg.contains("not yet confirmed on real hardware"), "{msg}");
    }
    for fw in NO_POWER {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::NoPath, "{fw:?}: {x:?}");
        assert!(x.paths_tried[0].contains("supports_set_power_limit() = false"), "{x:?}");
    }
    // Bitaxe: asic-rs has no power control; the AxeOS override maps power to
    // a frequency stand-in (PR H), and 2500 W is far outside the Bitaxe range.
    let x = &o[&IpAddr::from(AXE)];
    assert_eq!(x.status, WriteStatus::Refused, "{x:?}");
    assert!(x.msg.as_deref().unwrap().contains("power_w 2500 outside 0..=40"), "{x:?}");
    assert!(x.paths_tried[0].starts_with("asic-rs: skipped (asic-rs: power limit not supported"), "{x:?}");
    assert!(x.paths_tried[1].starts_with("axeos-http: selected for dry run only"), "{x:?}");
    assert!(alert.is_none());
    for m in r.miners.values() {
        assert!(m.writes().is_empty(), "{:?}: dry run wrote", m.fw);
    }
    assert!(!r.axe.as_ref().unwrap().requests().iter().any(|q| q.starts_with("PATCH")), "nothing sent to the Bitaxe");
}

/// A live write with the app's default options is refused (no backend is
/// hardware-confirmed) and touches nothing.
#[tokio::test(start_paused = true)]
async fn power_live_write_needs_a_confirmed_path() {
    let r = rig(false, false).await;
    let (o, _) = run(&r, all_native(), power(2500), false, None).await;
    for fw in POWER_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::NoPath, "{fw:?}");
        assert_eq!(x.paths_tried, ["asic-rs: skipped (not yet confirmed on real hardware)"]);
        assert!(r.m(fw).writes().is_empty());
    }
}

/// Mock-only opt-in: every power-capable firmware is written once, read
/// back, watched, verified, snapshotted and audited.
#[tokio::test(start_paused = true)]
async fn power_limit_fleetwide_verified_with_snapshot_and_audit() {
    let r = rig(true, false).await;
    let (o, alert) = run(&r, all_native(), power(2500), false, Some(2)).await;
    for fw in POWER_CAPABLE {
        let x = &o[&ip_of(fw)];
        assert_eq!(x.status, WriteStatus::Verified, "{fw:?}: {x:?}");
        assert_eq!(x.paths_tried, ["asic-rs: selected"]);
        assert_eq!(r.m(fw).power(), 2500);
        assert_eq!(r.m(fw).writes(), ["set_power_limit 2500"], "{fw:?}: exactly one write");
        assert_eq!(x.before, Some(power(3000)));
        assert_eq!(x.after, Some(power(2500)));
        let snap = r.engine.snapshots.get(x.snapshot_id.as_deref().unwrap()).unwrap();
        assert_eq!(snap.settings, power(3000));
    }
    for fw in NO_POWER {
        assert_eq!(o[&ip_of(fw)].status, WriteStatus::NoPath);
        assert!(r.m(fw).writes().is_empty(), "{fw:?}: no write without a path");
    }
    assert!(alert.is_none());
    let recs = r.audit.records();
    assert_eq!(recs.iter().filter(|x| x.phase == "attempt").count(), POWER_CAPABLE.len());
    assert_eq!(recs.iter().filter(|x| x.phase == "outcome").count(), all_native().len());
    assert!(recs.iter().all(|x| x.action == "apply_settings" && x.requested == Some(serde_json::json!({"power_limit_w": 2500}))));
}

#[tokio::test(start_paused = true)]
async fn power_and_tuning_limits_refuse_before_any_write() {
    let r = rig(true, false).await;
    let braiins = ip_of(NativeFirmware::Braiins2507);
    let vnish = ip_of(NativeFirmware::Vnish13);
    let (o, _) = run(&r, vec![braiins, vnish], power(5000), false, None).await;
    for ip in [braiins, vnish] {
        assert_eq!(o[&ip].status, WriteStatus::Refused, "{:?}", o[&ip]);
        assert!(o[&ip].msg.as_deref().unwrap().contains("power_w 5000 outside 1000..=3250"), "{:?}", o[&ip]);
    }
    let (o, _) = run(&r, vec![vnish], pct(20), false, None).await;
    assert_eq!(o[&vnish].status, WriteStatus::Refused);
    assert!(o[&vnish].msg.as_deref().unwrap().contains("tuning_pct 20 outside 50..=100"));
    assert!(r.m(NativeFirmware::Braiins2507).writes().is_empty() && r.m(NativeFirmware::Vnish13).writes().is_empty());
}

#[tokio::test(start_paused = true)]
async fn tuning_percent_only_where_asic_rs_supports_it() {
    let r = rig(true, false).await;
    let vnish = ip_of(NativeFirmware::Vnish13);
    let braiins = ip_of(NativeFirmware::Braiins2507);
    let (o, _) = run(&r, vec![vnish, braiins], pct(80), false, None).await;
    assert_eq!(o[&vnish].status, WriteStatus::Verified, "{:?}", o[&vnish]);
    assert_eq!(r.m(NativeFirmware::Vnish13).tuning(), 80);
    assert_eq!(r.m(NativeFirmware::Vnish13).writes(), ["set_tuning_percent 80"]);
    assert_eq!(o[&braiins].status, WriteStatus::NoPath);
    assert!(o[&braiins].paths_tried[0].contains("supports_set_tuning_percent() = false"), "{:?}", o[&braiins]);
    assert!(r.m(NativeFirmware::Braiins2507).writes().is_empty());
    // Both in one change on VNish: one snapshot, both restored on rollback.
    r.m(NativeFirmware::Vnish13).set_unhealthy_below(Some(2000));
    let both = Settings { power_limit_w: Some(1500), tuning_pct: Some(90), ..Default::default() };
    let (o, _) = run(&r, vec![vnish], both, false, None).await;
    assert_eq!(o[&vnish].status, WriteStatus::RolledBack, "{:?}", o[&vnish]);
    assert_eq!(o[&vnish].before, Some(Settings { power_limit_w: Some(3000), tuning_pct: Some(80), ..Default::default() }));
    let m = r.m(NativeFirmware::Vnish13);
    assert_eq!((m.power(), m.tuning()), (3000, 80));
}

/// Health watch → rollback → aggregated alert; the canary halts the rest.
#[tokio::test(start_paused = true)]
async fn power_rollback_and_canary_halt() {
    let r = rig(true, false).await;
    for fw in POWER_CAPABLE {
        r.m(fw).set_unhealthy_below(Some(2000));
    }
    let ips: Vec<IpAddr> = POWER_CAPABLE.iter().map(|f| ip_of(*f)).collect();
    let (o, alert) = run(&r, ips.clone(), power(1500), false, Some(2)).await;
    let first: Vec<IpAddr> = ips[..2].to_vec();
    for ip in &first {
        assert_eq!(o[ip].status, WriteStatus::RolledBack, "{:?}", o[ip]);
        assert_eq!(o[ip].cause, Some(FailureCause::HashrateDrop));
        assert_eq!(o[ip].after, Some(power(3000)), "restored to the snapshot");
    }
    for fw in &POWER_CAPABLE[..2] {
        assert_eq!(r.m(*fw).power(), 3000);
        assert_eq!(r.m(*fw).writes(), ["set_power_limit 1500", "set_power_limit 3000"], "change + rollback");
    }
    for fw in &POWER_CAPABLE[2..] {
        let x = &o[&ip_of(*fw)];
        assert_eq!((x.status, x.skip_reason.as_deref()), (WriteStatus::Skipped, Some("halted")), "{x:?}");
        assert!(r.m(*fw).writes().is_empty(), "{fw:?}: halted miners are never touched");
    }
    let a = alert.expect("alert");
    assert!(a.headline.starts_with("2 of 7 rolled back, 5 halted"), "{}", a.headline);
    assert_eq!(a.groups[0].cause, FailureCause::HashrateDrop);
    assert_eq!(a.groups[0].miners[0].attempted, Some(power(1500)));
    assert!(a.retry.dry_run, "retry is offered as a dry run");
}

#[tokio::test(start_paused = true)]
async fn power_read_back_mismatch_rolls_back() {
    let r = rig(true, false).await;
    let fw = NativeFirmware::WhatsminerV3;
    r.m(fw).set_ignore_writes(true);
    let (o, _) = run(&r, vec![ip_of(fw)], power(2500), false, None).await;
    let x = &o[&ip_of(fw)];
    assert_eq!(x.status, WriteStatus::RolledBack, "{x:?}");
    assert_eq!(x.cause, Some(FailureCause::ReadBackMismatch));
    assert!(x.msg.as_deref().unwrap().contains("power_limit_w: got Some(3000), want 2500"), "{x:?}");
}

/// The dialog's support / limits view, per device class.
#[tokio::test(flavor = "multi_thread")]
async fn settings_support_rows_show_limits_and_paths() {
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
    assert_eq!((v.power_w, v.tuning_pct), (Some([1000, 3250]), Some([50, 100])));
    assert_eq!(v.power.path.as_deref(), Some("asic-rs"));
    assert!(!v.power.confirmed, "dry run only until hardware-confirmed");
    assert_eq!(v.tuning.path.as_deref(), Some("asic-rs"));
    let a = row(ip_of(NativeFirmware::AntminerStock)).await;
    assert_eq!(a.power.path, None);
    assert!(a.power.tried[0].contains("supports_set_power_limit() = false"));
    let b = row(IpAddr::from(AXE)).await;
    assert_eq!(b.class, Some(super::DeviceClass::AxeOs));
    // Power maps to the AxeOS frequency stand-in; tuning percent has no AxeOS equivalent.
    assert_eq!((b.power.path.as_deref(), b.tuning.path.clone()), (Some("axeos-http"), None));
    assert_eq!(b.power_w, Some([0, 40]));
    assert!(b.tuning.tried.iter().any(|t| t.contains("tuning_pct has no AxeOS equivalent")), "{:?}", b.tuning.tried);
    let missing = crate::control::settings_support_row(&r.engine, IpAddr::from([192, 0, 2, 254]), None).await;
    assert!(!missing.found);
    let json = serde_json::to_value(&v).unwrap();
    assert_eq!(json["power"]["path"], "asic-rs");
    assert_eq!(json["tuning_pct"], serde_json::json!([50, 100]));
}

#[test]
fn new_classes_map_from_asic_rs_makes() {
    use super::profile::{classify_class, DeviceClass};
    assert_eq!(classify_class("Proto", "Proto v1"), DeviceClass::Proto);
    assert_eq!(classify_class("SealMiner", "v2025"), DeviceClass::Sealminer);
    assert_eq!(classify_class("Auradine", "v1"), DeviceClass::Auradine);
    assert_eq!(classify_class("AvalonMiner", "Avalon A"), DeviceClass::Avalon);
    assert_eq!(classify_class("AntMiner", "VNish 1.3"), DeviceClass::Vnish);
}

#[test]
fn every_power_capable_mock_has_limits() {
    let t = super::limits::LimitsTable::builtin();
    for fw in POWER_CAPABLE {
        let (make, model, f) = fw.identity();
        let p = super::DeviceProfile::classify(make, model, f, None);
        assert!(t.lookup(&p).and_then(|e| e.power_w).is_some(), "{fw:?} ({p:?}) has no power_w range");
    }
}
