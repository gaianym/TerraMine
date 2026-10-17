//! End-to-end: safety engine / guarded job runner → real HTTP drivers →
//! stateful mock miners (no real hardware).

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::audit::MemoryAudit;
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, WriteStatus};
use super::health::HealthPolicy;
use super::policy::Confirmation;
use super::profile::{DeviceClass, DeviceProfile};
use super::settings::Settings;
use super::snapshot::SnapshotStore;
use crate::control::driver::MinerAction;
use crate::control::job::{run_action_guarded, ActionRowEvent, ActionStatus, JobConfig, SafetyCtx, Sink, VerifyConfig};
use crate::control::mock_miner::{Fault, MockKind, MockMiner, MockMinerFactory, MOCK_CORE_MV, MOCK_FREQ_MHZ};

const AXE: [u8; 4] = [192, 0, 2, 20];
const S9: [u8; 4] = [192, 0, 2, 21];

struct Rig {
    axe: MockMiner,
    s9: MockMiner,
    factory: Arc<MockMinerFactory>,
    audit: Arc<MemoryAudit>,
    engine: SafetyEngine,
}

fn opts(allow_unconfirmed: bool) -> SafetyOptions {
    SafetyOptions {
        op_timeout: Duration::from_secs(2),
        readback_timeout: Duration::from_secs(2),
        health: HealthPolicy {
            window: Duration::from_millis(500),
            interval: Duration::from_millis(50),
            settle: Duration::from_millis(150),
            min_hashrate_ratio: 0.8,
            max_temp_c: None,
            max_unreachable_polls: 3,
        },
        rollback_timeout: Duration::from_secs(8),
        allow_unconfirmed_paths: allow_unconfirmed,
    }
}

async fn rig_with(allow_unconfirmed: bool) -> Rig {
    let down = Duration::from_millis(600);
    let axe = MockMiner::spawn(MockKind::AxeOs215, down).await.unwrap();
    let s9 = MockMiner::spawn(MockKind::S9Stock { miner_mode: true }, down).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    f.add(IpAddr::from(AXE), &axe);
    f.add(IpAddr::from(S9), &s9);
    let audit = Arc::new(MemoryAudit::default());
    let engine = SafetyEngine::new(audit.clone(), Arc::new(SnapshotStore::in_memory(32)), opts(allow_unconfirmed));
    Rig { axe, s9, factory: Arc::new(f), audit, engine }
}

async fn rig() -> Rig {
    rig_with(true).await
}

fn apply(change: Settings) -> SafeWriteRequest {
    SafeWriteRequest { change, dry_run: false, confirm: Confirmation::default() }
}

fn freq(f: u32) -> Settings {
    Settings { frequency_mhz: Some(f), ..Default::default() }
}

const HEALTHY: (u32, u32) = (MOCK_FREQ_MHZ, MOCK_CORE_MV);

#[tokio::test(flavor = "multi_thread")]
async fn e2e_healthy_change_is_verified_and_sticks() {
    let r = rig().await;
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let change = Settings { frequency_mhz: Some(575), core_voltage_mv: Some(1200), ..Default::default() };
    let o = r.engine.apply_settings("run", &t, &apply(change)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(r.axe.settings(), (575, 1200));
    assert_eq!(r.axe.writes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollback_restores_snapshot_when_hashrate_drops() {
    let r = rig().await;
    r.axe.set_fault(Some(Fault::HashrateDrop));
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("hashrate"), "{o:?}");
    assert_eq!(r.axe.settings(), HEALTHY, "device back on snapshot values");
    assert_eq!(r.axe.writes(), 2, "change + rollback");
    let snap = r.engine.snapshots.get(o.snapshot_id.as_deref().unwrap()).unwrap();
    assert_eq!(snap.settings, freq(MOCK_FREQ_MHZ));
    let recs = r.audit.records();
    assert_eq!(recs.len(), 2);
    assert_eq!((recs[0].phase.as_str(), recs[0].outcome.as_str()), ("attempt", "pending"));
    assert_eq!((recs[1].phase.as_str(), recs[1].outcome.as_str()), ("outcome", "rolled_back"));
    assert_eq!(recs[1].snapshot_id, o.snapshot_id);
    assert_eq!(recs[1].device_class, Some(DeviceClass::AxeOs));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollback_when_overheating() {
    let r = rig().await;
    r.axe.set_fault(Some(Fault::Overheat));
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(Settings { core_voltage_mv: Some(1250), ..Default::default() })).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("temperature 95.0°C above 70.0°C"), "{o:?}");
    assert_eq!(r.axe.settings(), HEALTHY);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollback_after_device_drops_off_the_network() {
    let r = rig().await;
    r.axe.set_fault(Some(Fault::Unreachable));
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(r.axe.settings(), HEALTHY);
    assert!(r.axe.requests().iter().any(|q| q == "PATCH /api/system"));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_read_back_mismatch_rolls_back() {
    let r = rig().await;
    r.axe.set_ignore_writes(true);
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert!(o.msg.as_deref().unwrap().contains("read-back failed"));
    assert_eq!(r.axe.settings(), HEALTHY);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_dry_run_and_limit_refusal_send_no_writes() {
    let r = rig().await;
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let dry = SafeWriteRequest { change: freq(575), dry_run: true, confirm: Confirmation::default() };
    let o = r.engine.apply_settings("run", &t, &dry).await;
    assert_eq!(o.status, WriteStatus::DryRun);
    assert_eq!(o.msg.as_deref(), Some("would set frequency_mhz 525 → 575"));
    let before = r.axe.requests().len();
    let o = r.engine.apply_settings("run", &t, &apply(freq(900))).await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert_eq!(r.axe.requests().len(), before, "limit refusal sends nothing at all");
    assert_eq!(r.axe.writes(), 0);
    assert_eq!(r.axe.settings(), HEALTHY);
}

/// Permanent: the AxeOS (Bitaxe) path never hits a non-AxeOS device.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_axeos_path_never_hits_non_axeos_mock() {
    let r = rig().await;
    let t = r.factory.target(IpAddr::from(S9)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert!(r.s9.requests().is_empty(), "S9 saw {:?}", r.s9.requests());

    // Even a mislabeled profile (says AxeOS, endpoint is an S9) is caught
    // by the read-only identity probe before any write.
    let mut wrong = t.clone();
    wrong.profile = DeviceProfile::classify("Bitaxe", "Bitaxe", "AxeOS", Some("BM1370"));
    let o = r.engine.apply_settings("run", &wrong, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    let reqs = r.s9.requests();
    assert!(reqs.iter().all(|q| q == "GET /api/system/info"), "only the identity probe: {reqs:?}");
    assert!(o.paths_tried[0].starts_with("axeos-http: skipped (AxeOS info probe failed"), "{o:?}");
}

/// Permanent: the cgminer path never hits an AxeOS device.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_cgminer_path_never_hits_axeos_mock() {
    let r = rig().await;
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(o.paths_tried, vec!["axeos-http: opened".to_string()], "cgminer path never considered");
    let mut wrong = t.clone();
    wrong.profile = DeviceProfile::classify("AntMiner", "Antminer S9", "Stock", None);
    let before = r.axe.requests().len();
    let o = r.engine.apply_settings("run", &wrong, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    let reqs = r.axe.requests();
    assert_eq!(reqs.len(), before, "nothing sent to the AxeOS mock: {reqs:?}");
    assert!(!reqs.iter().any(|q| q == "cgminer" || q == "unparsed"));
}

/// Permanent: total failure lists every path tried, each with a reason.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_total_failure_reports_paths_tried() {
    let r = rig().await;
    let t = r.factory.target(IpAddr::from(S9)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath);
    assert_eq!(o.paths_tried.len(), 2, "{o:?}");
    assert!(o.paths_tried[0].starts_with("axeos-http: skipped (device class AntminerStock is not AxeOS)"));
    assert!(o.paths_tried[1].starts_with("cgminer-rpc: skipped ("));
    let msg = o.msg.unwrap();
    assert!(msg.contains("tried:") && msg.contains("axeos-http") && msg.contains("cgminer-rpc"), "{msg}");
    assert_eq!(r.audit.records().last().unwrap().paths_tried, o.paths_tried);
}

/// Default options: no path is hardware-confirmed, so nothing is written.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_default_options_never_write_through_unconfirmed_paths() {
    let r = rig_with(false).await;
    let t = r.factory.target(IpAddr::from(AXE)).unwrap();
    let o = r.engine.apply_settings("run", &t, &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert!(o.paths_tried[0].contains("not yet confirmed on real hardware"));
    assert!(r.axe.requests().is_empty());
}

// ---------------- phase-1 through the safety layer ----------------

fn sink() -> (Sink, Arc<Mutex<Vec<ActionRowEvent>>>) {
    let v = Arc::new(Mutex::new(Vec::new()));
    let v2 = v.clone();
    (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
}

fn cfg(action: MinerAction, dry_run: bool) -> JobConfig {
    JobConfig {
        action,
        concurrency: 4,
        stagger: Duration::from_millis(10),
        timeout: Duration::from_secs(2),
        dry_run,
        verify: Some(VerifyConfig { interval: Duration::from_millis(200), timeout: Duration::from_secs(10) }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_phase1_reboot_unchanged_and_audited() {
    let r = rig().await;
    let (s, events) = sink();
    let ctx = SafetyCtx { run_id: "run-1".into(), audit: r.audit.clone() };
    let ips = vec![IpAddr::from(AXE), IpAddr::from(S9)];
    let res = run_action_guarded(ips, cfg(MinerAction::Reboot, false), r.factory.clone(), CancellationToken::new(), s, ctx).await;
    assert!(res.iter().all(|e| e.status == ActionStatus::Verified), "{res:?}");
    for ip in ["192.0.2.20", "192.0.2.21"] {
        let seq: Vec<_> = events.lock().unwrap().iter().filter(|e| e.ip == ip).map(|e| e.status).collect();
        assert_eq!(seq, vec![ActionStatus::Queued, ActionStatus::Sent, ActionStatus::Verifying, ActionStatus::Verified]);
        let recs: Vec<_> = r.audit.records().into_iter().filter(|a| a.ip == ip).collect();
        assert_eq!(recs.iter().map(|a| (a.phase.as_str(), a.outcome.as_str())).collect::<Vec<_>>(), [("attempt", "sent"), ("outcome", "verified")]);
        assert!(recs.iter().all(|a| a.run_id == "run-1" && a.action == "reboot" && !a.dry_run));
        assert!(recs[0].pre_state.as_ref().unwrap()["uptime_s"].as_u64().unwrap() >= 3600, "baseline uptime recorded");
    }
    let classes: Vec<_> = r.audit.records().iter().filter(|a| a.phase == "outcome").map(|a| a.device_class).collect();
    assert!(classes.contains(&Some(DeviceClass::AxeOs)) && classes.contains(&Some(DeviceClass::AntminerStock)));
    assert_eq!((r.axe.reboots(), r.s9.reboots()), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_phase1_dry_run_audited_without_attempt() {
    let r = rig().await;
    let (s, _) = sink();
    let ctx = SafetyCtx { run_id: "run-2".into(), audit: r.audit.clone() };
    let res = run_action_guarded(vec![IpAddr::from(AXE)], cfg(MinerAction::Stop, true), r.factory.clone(), CancellationToken::new(), s, ctx).await;
    assert_eq!(res[0].status, ActionStatus::DryRun);
    assert!(!r.axe.paused());
    let recs = r.audit.records();
    assert_eq!(recs.len(), 1);
    assert_eq!((recs[0].phase.as_str(), recs[0].outcome.as_str(), recs[0].dry_run), ("outcome", "dryrun", true));
}
