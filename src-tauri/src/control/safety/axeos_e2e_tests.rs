//! Plan PR H: AxeOS (Bitaxe) frequency / core voltage / fan / target
//! temperature through the safety engine, against the restart-to-apply
//! AxeOS mock (no real hardware).

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::audit::MemoryAudit;
use super::axeos::{fan_key, frequency_applied, parse_axeos_settings, power_stand_in};
use super::batch::{run_settings_job, BatchOptions};
use super::engine::{raise_hold, SafeWriteRequest, SafetyEngine, SafetyOptions, Target, WriteOutcome, WriteStatus};
use super::health::{FailureCause, HealthPolicy};
use super::policy::Confirmation;
use super::settings::Settings;
use super::snapshot::SnapshotStore;
use crate::control::cache::{DeviceCache, Resolved, Resolver};
use crate::control::mock_miner::{Fault, MockKind, MockMiner, MockMinerFactory, MOCK_CORE_MV, MOCK_FREQ_MHZ};

const AXE: [u8; 4] = [192, 0, 2, 40];
const OLD: [u8; 4] = [192, 0, 2, 41];
const S9: [u8; 4] = [192, 0, 2, 42];

struct Rig {
    axe: MockMiner,
    old: MockMiner,
    s9: MockMiner,
    resolver: Arc<Resolver>,
    engine: Arc<SafetyEngine>,
}

fn opts(allow_unconfirmed: bool) -> SafetyOptions {
    SafetyOptions {
        op_timeout: Duration::from_secs(2),
        readback_timeout: Duration::from_secs(3),
        health: HealthPolicy {
            window: Duration::from_millis(500),
            interval: Duration::from_millis(50),
            settle: Duration::from_millis(150),
            min_hashrate_ratio: 0.8,
            max_temp_c: None,
            max_unreachable_polls: 3,
        },
        rollback_timeout: Duration::from_secs(4),
        allow_unconfirmed_paths: allow_unconfirmed,
    }
}

async fn rig_with(allow_unconfirmed: bool) -> Rig {
    let down = Duration::from_millis(400);
    let axe = MockMiner::spawn(MockKind::AxeOs215, down).await.unwrap();
    let old = MockMiner::spawn(MockKind::AxeOs210, down).await.unwrap();
    let s9 = MockMiner::spawn(MockKind::S9Stock { miner_mode: true }, down).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    f.add(IpAddr::from(AXE), &axe);
    f.add(IpAddr::from(OLD), &old);
    f.add(IpAddr::from(S9), &s9);
    let engine = Arc::new(SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(32)), opts(allow_unconfirmed)));
    let resolver = Arc::new(Resolver::new(Arc::new(f), Arc::new(DeviceCache::default())));
    Rig { axe, old, s9, resolver, engine }
}

async fn target(r: &Rig, ip: [u8; 4]) -> Target {
    let ip = IpAddr::from(ip);
    match r.resolver.resolve(ip, Duration::from_secs(2)).await {
        Resolved::Ready { driver, .. } => Target::new(ip, driver),
        _ => panic!("mock {ip} did not resolve"),
    }
}

fn req(change: Settings, dry_run: bool, raise: bool) -> SafeWriteRequest {
    SafeWriteRequest { change, dry_run, confirm: Confirmation { allow_frequency_raise: raise, ..Default::default() } }
}

fn freq(f: u32) -> Settings {
    Settings { frequency_mhz: Some(f), ..Default::default() }
}

fn power(w: u32) -> Settings {
    Settings { power_limit_w: Some(w), ..Default::default() }
}

fn api_calls(m: &MockMiner) -> Vec<String> {
    m.requests().into_iter().filter(|q| q.contains("/api/system") && q != "GET /api/system/info").collect()
}

const HEALTHY: (u32, u32) = (MOCK_FREQ_MHZ, MOCK_CORE_MV);

/// The mock applies frequency / voltage only on restart, like AxeOS.
#[tokio::test(flavor = "multi_thread")]
async fn mock_axeos_applies_frequency_only_after_restart() {
    let m = MockMiner::spawn(MockKind::AxeOs215, Duration::from_millis(200)).await.unwrap();
    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    c.patch(format!("{}/api/system", m.base())).json(&json!({"frequency": 500, "coreVoltage": 1100})).send().await.unwrap();
    assert_eq!(m.stored(), (500, 1100));
    assert_eq!(m.settings(), HEALTHY, "not running before the restart");
    let info: serde_json::Value = c.get(format!("{}/api/system/info", m.base())).send().await.unwrap().json().await.unwrap();
    assert_eq!((info["frequency"].as_u64(), info["actualFrequency"].as_u64()), (Some(500), Some(525)));
    c.post(format!("{}/api/system/restart", m.base())).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info: serde_json::Value = c.get(format!("{}/api/system/info", m.base())).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["actualFrequency"].as_u64(), Some(500));
    assert_eq!(m.settings(), (500, 1100));
}

/// Frequency + voltage: PATCH, restart as part of the write, wait, read
/// back after the restart, health watch, verified.
#[tokio::test(flavor = "multi_thread")]
async fn frequency_and_voltage_restart_to_apply_then_verified() {
    let r = rig_with(true).await;
    let t = target(&r, AXE).await;
    let change = Settings { frequency_mhz: Some(500), core_voltage_mv: Some(1100), ..Default::default() };
    let o = r.engine.apply_settings("h", &t, &req(change.clone(), false, false)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.axe.settings(), (500, 1100), "running after the restart");
    assert_eq!(r.axe.reboots(), 1);
    assert_eq!(api_calls(&r.axe), ["PATCH /api/system", "POST /api/system/restart"]);
    assert_eq!(o.before, Some(Settings { frequency_mhz: Some(525), core_voltage_mv: Some(1150), ..Default::default() }));
    assert_eq!(o.after, Some(change));
    assert!(o.msg.unwrap().contains("applied frequency_mhz 525 → 500, core_voltage_mv 1150 → 1100"));
}

/// Rollback = PATCH the snapshot + restart again; the health watch spans
/// the restart (overheat while coming back up is caught there).
#[tokio::test(flavor = "multi_thread")]
async fn rollback_patches_snapshot_and_restarts() {
    let r = rig_with(true).await;
    r.axe.set_fault(Some(Fault::Overheat));
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("h", &t, &req(freq(500), false, false)).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::Overheat));
    assert!(o.msg.as_deref().unwrap().contains("during restart: temperature 95.0°C above 70.0°C"), "{o:?}");
    assert_eq!(r.axe.settings(), HEALTHY);
    assert_eq!(r.axe.stored(), HEALTHY);
    assert_eq!(r.axe.reboots(), 2, "restart to apply + restart to roll back");
    assert_eq!(
        api_calls(&r.axe),
        ["PATCH /api/system", "POST /api/system/restart", "PATCH /api/system", "POST /api/system/restart"]
    );
}

/// The configured value reads back at once on AxeOS; only the running
/// `actualFrequency` proves the change applied.
#[tokio::test(flavor = "multi_thread")]
async fn read_back_checks_running_frequency_after_restart() {
    let r = rig_with(true).await;
    r.axe.set_frequency(430);
    r.axe.set_freq_cap(Some(450)); // firmware throttles anything above 450
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("h", &t, &req(freq(500), false, true)).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::ReadBackMismatch));
    assert!(o.msg.as_deref().unwrap().contains("actualFrequency 450 MHz, want 500 MHz"), "{o:?}");
    assert_eq!(r.axe.stored().0, 430);
    assert_eq!(r.axe.settings().0, 430);
}

/// A restart that never happens is a failed write; rollback still
/// succeeds because the snapshot is stored and still running.
#[tokio::test(flavor = "multi_thread")]
async fn restart_not_observed_fails_the_write() {
    let r = rig_with(true).await;
    r.axe.set_ignore_restart(true);
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("h", &t, &req(freq(500), false, false)).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::WriteFailed));
    assert!(o.msg.as_deref().unwrap().contains("restart to apply not observed (uptime did not reset)"), "{o:?}");
    assert_eq!(r.axe.stored(), HEALTHY);
    assert_eq!(r.axe.settings(), HEALTHY);
}

/// Permanent (bug): on AxeOS >= 2.11 the manual fan setting is
/// `manualFanSpeed`; `fanspeed` is the measured speed and a PATCH of it is
/// ignored, so the old body never changed the fan.
#[tokio::test(flavor = "multi_thread")]
async fn manual_fan_uses_manual_fan_speed_on_new_axeos_without_restart() {
    let r = rig_with(true).await;
    let t = target(&r, AXE).await;
    let change = Settings { fan_pct: Some(80), auto_fan: Some(false), ..Default::default() };
    let o = r.engine.apply_settings("h", &t, &req(change, false, false)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.axe.fan(), (false, 80, 60));
    assert_eq!(r.axe.reboots(), 0, "fan applies live");
    assert_eq!(api_calls(&r.axe), ["PATCH /api/system"]);
    // Legacy AxeOS keeps `fanspeed`.
    let t = target(&r, OLD).await;
    let o = r.engine.apply_settings("h", &t, &req(Settings { fan_pct: Some(70), auto_fan: Some(false), ..Default::default() }, false, false)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.old.fan(), (false, 70, 60));
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_fan_with_target_temperature() {
    let r = rig_with(true).await;
    let t = target(&r, AXE).await;
    let change = Settings { auto_fan: Some(true), temp_target_c: Some(58), ..Default::default() };
    let o = r.engine.apply_settings("h", &t, &req(change, false, false)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.axe.fan(), (true, 60, 58));
    assert_eq!(r.axe.reboots(), 0);
    assert_eq!(o.before, Some(Settings { auto_fan: Some(true), temp_target_c: Some(60), ..Default::default() }));
}

/// Placeholder limits: BM1370 400-625 MHz, 1000-1250 mV, fan 20-100 %,
/// target 45-65 °C. Refusals send nothing.
#[tokio::test(flavor = "multi_thread")]
async fn limits_refuse_out_of_range_values_without_sending() {
    let r = rig_with(true).await;
    let t = target(&r, AXE).await;
    for (change, want) in [
        (freq(650), "frequency_mhz 650 outside 400..=625 for BM1370"),
        (freq(350), "frequency_mhz 350 outside 400..=625"),
        (Settings { core_voltage_mv: Some(1300), ..Default::default() }, "core_voltage_mv 1300 outside 1000..=1250"),
        (Settings { core_voltage_mv: Some(950), ..Default::default() }, "core_voltage_mv 950 outside 1000..=1250"),
        (Settings { fan_pct: Some(10), ..Default::default() }, "fan_pct 10 outside 20..=100"),
        (Settings { temp_target_c: Some(70), ..Default::default() }, "temp_target_c 70 outside 45..=65"),
    ] {
        let o = r.engine.apply_settings("h", &t, &req(change, false, true)).await;
        assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
        assert!(o.msg.as_deref().unwrap().contains(want), "{o:?}");
    }
    assert!(api_calls(&r.axe).is_empty());
}

/// Units held low (e.g. 50 MHz after overheat throttling) stay held: a
/// raise needs an explicit OK, lowering does not.
#[tokio::test(flavor = "multi_thread")]
async fn frequency_raise_is_held_without_explicit_ok() {
    let r = rig_with(true).await;
    r.axe.set_frequency(50);
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("h", &t, &req(freq(450), true, false)).await;
    assert_eq!(o.status, WriteStatus::DryRun);
    assert!(o.msg.as_deref().unwrap().contains("frequency raise 50 → 450 MHz is held"), "{o:?}");
    let o = r.engine.apply_settings("h", &t, &req(freq(450), false, false)).await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    assert!(api_calls(&r.axe).is_empty(), "held: nothing sent");
    assert_eq!(r.axe.settings().0, 50);
    let o = r.engine.apply_settings("h", &t, &req(freq(450), false, true)).await;
    assert_eq!(o.status, WriteStatus::Verified, "explicit OK: {o:?}");
    // Lowering needs no OK.
    let o = r.engine.apply_settings("h", &t, &req(freq(425), false, false)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
}

/// Bitaxe "power tuning": power_limit_w maps to a frequency stand-in shown
/// in the plan text, then goes through the same limits / hold / restart.
#[tokio::test(flavor = "multi_thread")]
async fn power_request_maps_to_frequency_stand_in() {
    let r = rig_with(true).await;
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("h", &t, &req(power(15), true, false)).await;
    assert_eq!(o.status, WriteStatus::DryRun, "{o:?}");
    assert_eq!(
        o.msg.as_deref(),
        Some("power_limit_w 15 W: AxeOS has no power limit; stand-in frequency 525 → 445 MHz (estimated from 17.6 W at 525 MHz, core voltage unchanged); would set frequency_mhz 525 → 445 with a restart to apply via axeos-http")
    );
    assert_eq!(o.attempted, Some(freq(445)));
    let o = r.engine.apply_settings("h", &t, &req(power(15), false, false)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.axe.settings().0, 445);
    // Above the current draw = a frequency raise: held.
    let o = r.engine.apply_settings("h", &t, &req(power(20), false, false)).await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    assert!(o.msg.unwrap().contains("is held"));
    // Stand-in below the frequency floor: refused with the reason.
    let o = r.engine.apply_settings("h", &t, &req(power(10), true, false)).await;
    assert_eq!(o.status, WriteStatus::Refused, "{o:?}");
    let msg = o.msg.unwrap();
    assert!(msg.contains("stand-in frequency 445 → 295 MHz (estimated from 14.9 W at 445 MHz") && msg.contains("frequency_mhz 295 outside 400..=625"), "{msg}");
    // Power outside the model's power range: refused before mapping.
    let o = r.engine.apply_settings("h", &t, &req(power(60), true, false)).await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert!(o.msg.unwrap().contains("power_w 60 outside 0..=40"));
}

/// Same gate as every other path: with default options (nothing confirmed
/// on hardware) a dry run shows the plan, a live write sends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn live_axeos_writes_blocked_until_hardware_confirmed() {
    let r = rig_with(false).await;
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("h", &t, &req(freq(500), true, false)).await;
    assert_eq!(o.status, WriteStatus::DryRun);
    let msg = o.msg.unwrap();
    assert!(msg.ends_with("path not yet confirmed on real hardware, a live write is refused"), "{msg}");
    assert_eq!(o.paths_tried, ["axeos-http: selected for dry run only (not yet confirmed on real hardware)"]);
    for change in [freq(500), power(15), Settings { fan_pct: Some(80), ..Default::default() }] {
        let o = r.engine.apply_settings("h", &t, &req(change, false, true)).await;
        assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
        assert_eq!(o.paths_tried, ["axeos-http: skipped (not yet confirmed on real hardware)"]);
    }
    assert!(api_calls(&r.axe).is_empty());
}

/// Permanent: AxeOS calls only ever reach AxeOS devices, and the S9 gets
/// no AxeOS request in a mixed fleet job.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_fleet_job_sends_axeos_calls_only_to_axeos() {
    let r = rig_with(true).await;
    let rows = Arc::new(Mutex::new(Vec::<String>::new()));
    let r2 = rows.clone();
    let sink: super::batch::OutcomeSink = Arc::new(move |o: &WriteOutcome| r2.lock().unwrap().push(o.ip.clone()));
    let ips = vec![IpAddr::from(AXE), IpAddr::from(OLD), IpAddr::from(S9)];
    let o = BatchOptions { concurrency: 3, stagger: Duration::from_millis(5), resolve_timeout: Duration::from_secs(2), ..Default::default() };
    let (out, alert) =
        run_settings_job("mix", ips, req(freq(500), false, false), r.resolver.clone(), r.engine.clone(), o, CancellationToken::new(), sink).await;
    assert_eq!(rows.lock().unwrap().len(), 3);
    assert_eq!(out[0].status, WriteStatus::Verified, "{:?}", out[0]);
    assert_eq!(out[1].status, WriteStatus::Verified, "{:?}", out[1]);
    assert_eq!(out[2].status, WriteStatus::NoPath, "{:?}", out[2]);
    assert!(alert.is_none(), "no rollbacks, no alert");
    assert_eq!((r.axe.reboots(), r.old.reboots()), (1, 1));
    assert!(!r.s9.requests().iter().any(|q| q.contains("/api/")), "{:?}", r.s9.requests());
    assert!(!r.axe.requests().iter().any(|q| q.contains("cgi")));
}

/// Canary: a bad first wave halts the rest; each rollback restarts.
#[tokio::test(flavor = "multi_thread")]
async fn canary_halts_after_restart_rollbacks() {
    let down = Duration::from_millis(300);
    let mut miners = Vec::new();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    let mut ips = Vec::new();
    for i in 0..4u8 {
        let m = MockMiner::spawn(MockKind::AxeOs215, down).await.unwrap();
        let ip = IpAddr::from([198, 51, 100, 60 + i]);
        f.add(ip, &m);
        ips.push(ip);
        miners.push(m);
    }
    miners[0].set_fault(Some(Fault::HashrateDrop));
    let engine = Arc::new(SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(32)), opts(true)));
    let resolver = Arc::new(Resolver::new(Arc::new(f), Arc::new(DeviceCache::default())));
    let sink: super::batch::OutcomeSink = Arc::new(|_| {});
    let o = BatchOptions { concurrency: 1, canary_size: Some(1), halt_threshold: 0.0, resolve_timeout: Duration::from_secs(2), ..Default::default() };
    let (out, alert) = run_settings_job("can", ips, req(freq(500), false, false), resolver, engine, o, CancellationToken::new(), sink).await;
    assert_eq!(out[0].status, WriteStatus::RolledBack, "{:?}", out[0]);
    assert!(out[1..].iter().all(|o| o.status == WriteStatus::Skipped), "{out:?}");
    let a = alert.expect("alert");
    assert_eq!(a.halted, 3);
    assert_eq!(miners[0].reboots(), 2);
    assert_eq!(miners[0].settings(), HEALTHY);
    assert!(miners[1..].iter().all(|m| m.reboots() == 0 && m.writes() == 0));
}

#[test]
fn axeos_parsing_and_helpers() {
    let new = json!({"frequency": 525.0, "coreVoltage": 1150, "fanspeed": 47, "manualFanSpeed": 80, "autofanspeed": 0, "temptarget": 60});
    assert_eq!(fan_key(&new), "manualFanSpeed");
    let s = parse_axeos_settings(&new);
    assert_eq!((s.fan_pct, s.temp_target_c, s.auto_fan), (Some(80), Some(60), Some(false)));
    let old = json!({"frequency": 525, "fanspeed": 70});
    assert_eq!((fan_key(&old), parse_axeos_settings(&old).fan_pct), ("fanspeed", Some(70)));
    // Stand-in math rounds down to 5 MHz.
    let info = json!({"power": 17.6, "frequency": 525});
    assert_eq!(power_stand_in(&power(15), &info).unwrap().0, freq(445));
    assert!(power_stand_in(&power(15), &json!({"power": 0.0, "frequency": 525})).unwrap_err().contains("paused"));
    // Applied tolerance: max(2 MHz, 1%).
    assert!(frequency_applied(&json!({"actualFrequency": 498.0}), &freq(500)).is_ok());
    assert!(frequency_applied(&json!({"actualFrequency": 490.0}), &freq(500)).is_err());
    assert!(frequency_applied(&json!({}), &freq(500)).is_ok(), "older firmware: configured value only");
    // Raise hold.
    let c = Confirmation::default();
    assert!(raise_hold(&freq(525), &freq(500), &c).is_none());
    assert!(raise_hold(&freq(525), &freq(550), &c).is_some());
    assert!(raise_hold(&Settings::default(), &freq(500), &c).unwrap().contains("current frequency unknown"));
    assert!(raise_hold(&freq(525), &freq(550), &Confirmation { allow_frequency_raise: true, ..Default::default() }).is_none());
}
