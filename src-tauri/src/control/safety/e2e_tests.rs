//! End-to-end: safety engine / batch runner / guarded job runner → real
//! HTTP drivers → stateful mock miners (no real hardware).

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::audit::MemoryAudit;
use super::batch::{run_settings_job, BatchOptions, JobAlert};
use super::engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, Target, WriteOutcome, WriteStatus};
use super::health::{FailureCause, HealthPolicy};
use super::policy::Confirmation;
use super::profile::{DeviceClass, DeviceProfile};
use super::settings::Settings;
use super::snapshot::SnapshotStore;
use crate::control::cache::{DeviceCache, Resolved, Resolver};
use crate::control::driver::MinerAction;
use crate::control::job::{run_action_guarded, ActionRowEvent, ActionStatus, JobConfig, SafetyCtx, Sink, VerifyConfig};
use crate::control::mock_miner::{Fault, MockKind, MockMiner, MockMinerFactory, MOCK_CORE_MV, MOCK_FREQ_MHZ};

const AXE: [u8; 4] = [192, 0, 2, 20];
const S9: [u8; 4] = [192, 0, 2, 21];

struct Rig {
    axe: MockMiner,
    s9: MockMiner,
    factory: Arc<MockMinerFactory>,
    cache: Arc<DeviceCache>,
    resolver: Arc<Resolver>,
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
    let factory = Arc::new(f);
    let cache = Arc::new(DeviceCache::default());
    let resolver = Arc::new(Resolver::new(factory.clone(), cache.clone()));
    Rig { axe, s9, factory, cache, resolver, audit, engine }
}

async fn rig() -> Rig {
    rig_with(true).await
}

async fn target(r: &Rig, ip: [u8; 4]) -> Target {
    let ip = IpAddr::from(ip);
    match r.resolver.resolve(ip, Duration::from_secs(2)).await {
        Resolved::Ready { driver, .. } => Target::new(ip, driver),
        _ => panic!("mock {ip} did not resolve"),
    }
}

fn apply(change: Settings) -> SafeWriteRequest {
    SafeWriteRequest { change, dry_run: false, confirm: raise_ok() }
}

/// These tests exercise other gates; the frequency-raise hold has its own.
fn raise_ok() -> Confirmation {
    Confirmation { allow_frequency_raise: true, ..Default::default() }
}

fn freq(f: u32) -> Settings {
    Settings { frequency_mhz: Some(f), ..Default::default() }
}

const HEALTHY: (u32, u32) = (MOCK_FREQ_MHZ, MOCK_CORE_MV);

#[tokio::test(flavor = "multi_thread")]
async fn e2e_healthy_change_is_verified_and_sticks() {
    let r = rig().await;
    let t = target(&r, AXE).await;
    let change = Settings { frequency_mhz: Some(575), core_voltage_mv: Some(1200), ..Default::default() };
    let o = r.engine.apply_settings("run", &t, &apply(change)).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(o.paths_tried, ["axeos-http: selected"]);
    assert_eq!(r.axe.settings(), (575, 1200));
    assert_eq!(r.axe.writes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollback_restores_snapshot_when_hashrate_drops() {
    let r = rig().await;
    r.axe.set_fault(Some(Fault::HashrateDrop));
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::HashrateDrop));
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
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("run", &t, &apply(Settings { core_voltage_mv: Some(1250), ..Default::default() })).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::Overheat));
    assert!(o.msg.as_deref().unwrap().contains("temperature 95.0°C above 70.0°C"), "{o:?}");
    assert_eq!(r.axe.settings(), HEALTHY);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollback_after_device_drops_off_the_network() {
    let r = rig().await;
    r.axe.set_fault(Some(Fault::Unreachable));
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(r.axe.settings(), HEALTHY);
    assert!(r.axe.requests().iter().any(|q| q == "PATCH /api/system"));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_read_back_mismatch_rolls_back() {
    let r = rig().await;
    r.axe.set_ignore_writes(true);
    let t = target(&r, AXE).await;
    let o = r.engine.apply_settings("run", &t, &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::RolledBack, "{o:?}");
    assert_eq!(o.cause, Some(FailureCause::ReadBackMismatch));
    assert!(o.msg.as_deref().unwrap().contains("read-back failed"));
    assert_eq!(r.axe.settings(), HEALTHY);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_dry_run_and_limit_refusal_send_no_writes() {
    let r = rig().await;
    let t = target(&r, AXE).await;
    let dry = SafeWriteRequest { change: freq(575), dry_run: true, confirm: Confirmation::default() };
    let o = r.engine.apply_settings("run", &t, &dry).await;
    assert_eq!(o.status, WriteStatus::DryRun);
    assert_eq!(
        o.msg.as_deref(),
        Some("would set frequency_mhz 525 → 575 with a restart to apply via axeos-http; frequency raise 525 → 575 MHz is held: needs an explicit OK (allow_frequency_raise)")
    );
    let before = r.axe.requests().len();
    let o = r.engine.apply_settings("run", &t, &apply(freq(900))).await;
    assert_eq!(o.status, WriteStatus::Refused);
    assert_eq!(r.axe.requests().len(), before, "limit refusal sends nothing at all");
    assert_eq!(r.axe.writes(), 0);
    assert_eq!(r.axe.settings(), HEALTHY);
}

/// Permanent: the AxeOS (Bitaxe) override never hits a non-AxeOS device,
/// even when the profile is mislabeled.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_axeos_path_never_hits_non_axeos_mock() {
    let r = rig().await;
    let t = target(&r, S9).await;
    let before = r.s9.requests().len();
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert_eq!(o.paths_tried, [format!("s9-cgi: skipped ({})", crate::control::overrides::s9::S9_NO_SETTINGS)]);
    let mut wrong = t.clone();
    wrong.profile = DeviceProfile::classify("Bitaxe", "Bitaxe", "AxeOS", Some("BM1370"));
    let o = r.engine.apply_settings("run", &wrong, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    let reqs = r.s9.requests();
    assert_eq!(reqs.len(), before, "nothing sent to the S9 by the settings path: {reqs:?}");
    assert!(!reqs.iter().any(|q| q.contains("/api/system")));
}

/// Permanent: an AxeOS device mislabeled as an S9 gets nothing.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_mislabeled_axeos_gets_no_write() {
    let r = rig().await;
    let t = target(&r, AXE).await;
    let mut wrong = t.clone();
    wrong.profile = DeviceProfile::classify("AntMiner", "Antminer S9", "Stock", None);
    let before = r.axe.requests().len();
    let o = r.engine.apply_settings("run", &wrong, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert!(o.paths_tried[0].starts_with("axeos-http: skipped (device class AntminerStock is not AxeOS)"), "{o:?}");
    let reqs = r.axe.requests();
    assert_eq!(reqs.len(), before, "nothing sent to the AxeOS mock: {reqs:?}");
    assert!(!reqs.iter().any(|q| q == "cgminer" || q == "unparsed"));
}

/// Permanent: a refusal lists every backend considered, each with a reason.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_total_failure_reports_paths_tried() {
    let r = rig().await;
    let t = target(&r, AXE).await;
    let both = Settings { power_limit_w: Some(20), frequency_mhz: Some(500), ..Default::default() };
    let o = r.engine.apply_settings("run", &t, &apply(both)).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert_eq!(
        o.paths_tried,
        ["axeos-http: skipped (power_limit_w is mapped to a frequency on AxeOS; send either power_limit_w or frequency_mhz)"]
    );
    assert!(o.msg.unwrap().contains("tried: axeos-http: skipped (power_limit_w"));
    let t = target(&r, S9).await;
    let o = r.engine.apply_settings("run", &t, &apply(freq(600))).await;
    assert_eq!(o.status, WriteStatus::NoPath);
    let msg = o.msg.clone().unwrap();
    assert!(msg.contains("tried:") && msg.contains("s9-cgi: skipped (stock S9 firmware"), "{msg}");
    assert_eq!(r.audit.records().last().unwrap().paths_tried, o.paths_tried);
}

/// Default options: no backend is hardware-confirmed, so nothing is written.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_default_options_never_write_through_unconfirmed_paths() {
    let r = rig_with(false).await;
    let t = target(&r, AXE).await;
    let before = r.axe.requests().len();
    let o = r.engine.apply_settings("run", &t, &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::NoPath, "{o:?}");
    assert!(o.paths_tried[0].contains("not yet confirmed on real hardware"));
    assert_eq!(r.axe.requests().len(), before);
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
        allow_unconfirmed: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_phase1_reboot_unchanged_and_audited() {
    let r = rig().await;
    let (s, events) = sink();
    let ctx = SafetyCtx { run_id: "run-1".into(), audit: r.audit.clone(), cache: r.cache.clone() };
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
    let ctx = SafetyCtx { run_id: "run-2".into(), audit: r.audit.clone(), cache: r.cache.clone() };
    let res = run_action_guarded(vec![IpAddr::from(AXE)], cfg(MinerAction::Stop, true), r.factory.clone(), CancellationToken::new(), s, ctx).await;
    assert_eq!(res[0].status, ActionStatus::DryRun);
    assert!(!r.axe.paused());
    let recs = r.audit.records();
    assert_eq!(recs.len(), 1);
    assert_eq!((recs[0].phase.as_str(), recs[0].outcome.as_str(), recs[0].dry_run), ("outcome", "dryrun", true));
}

fn ctx(r: &Rig, id: &str) -> SafetyCtx {
    SafetyCtx { run_id: id.into(), audit: r.audit.clone(), cache: r.cache.clone() }
}

/// Permanent: identity comes from the cache, not a fresh identify per
/// action or per verify poll.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_no_reidentify_per_action_or_poll() {
    let r = rig().await;
    let ips = vec![IpAddr::from(AXE), IpAddr::from(S9)];
    let (s, _) = sink();
    let res = run_action_guarded(ips.clone(), cfg(MinerAction::Reboot, false), r.factory.clone(), CancellationToken::new(), s, ctx(&r, "a")).await;
    assert!(res.iter().all(|e| e.status == ActionStatus::Verified), "{res:?}");
    assert_eq!(r.factory.connects(), 2, "one identify per miner for reboot + all verify polls");
    for action in [MinerAction::Stop, MinerAction::Start] {
        let (s, _) = sink();
        let res = run_action_guarded(ips.clone(), cfg(action, false), r.factory.clone(), CancellationToken::new(), s, ctx(&r, "b")).await;
        assert!(res.iter().all(|e| e.status == ActionStatus::Verified), "{action:?}: {res:?}");
    }
    assert_eq!(r.factory.connects(), 2, "later runs reuse the cached driver");
    assert_eq!(r.factory.builds(), 0);
    // The old per-action AxeOS detection GET is gone: a stop+start on the
    // cached driver sends no extra /api/system/info beyond the identity
    // check and verification reads.
    assert!(!r.axe.paused());
}

/// Permanent: a discovery-seeded cache needs no identification at all.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_discovery_cache_skips_identification() {
    let r = rig().await;
    for ip in [AXE, S9] {
        r.cache.put(IpAddr::from(ip), r.factory.discovered(IpAddr::from(ip)).unwrap());
    }
    let ips = vec![IpAddr::from(AXE), IpAddr::from(S9)];
    let (s, _) = sink();
    let res = run_action_guarded(ips, cfg(MinerAction::Reboot, false), r.factory.clone(), CancellationToken::new(), s, ctx(&r, "d")).await;
    assert!(res.iter().all(|e| e.status == ActionStatus::Verified), "{res:?}");
    assert_eq!(r.factory.connects(), 0, "no identify when discovery already knows the device");
    assert_eq!(r.factory.builds(), 2, "driver rebuilt from the cache record");
    assert_eq!((r.axe.reboots(), r.s9.reboots()), (1, 1));
}

/// Permanent: a different device at a cached IP is refused for that run,
/// re-identified once, and handled normally on the next run.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_identity_change_refuses_then_reidentifies_once() {
    let r = rig().await;
    let ip = vec![IpAddr::from(AXE)];
    let (s, _) = sink();
    run_action_guarded(ip.clone(), cfg(MinerAction::Stop, false), r.factory.clone(), CancellationToken::new(), s, ctx(&r, "1")).await;
    assert!(r.axe.paused());
    r.axe.set_mac("02:00:00:00:BE:EF");
    let (s, _) = sink();
    let res = run_action_guarded(ip.clone(), cfg(MinerAction::Start, false), r.factory.clone(), CancellationToken::new(), s, ctx(&r, "2")).await;
    assert_eq!(res[0].status, ActionStatus::Refused, "{res:?}");
    let msg = res[0].msg.clone().unwrap();
    assert!(msg.contains("changed (MAC") && msg.contains("02:00:00:00:BE:EF") && msg.contains("not sent this run"), "{msg}");
    assert!(r.axe.paused(), "nothing sent to the changed device");
    assert_eq!(r.factory.connects(), 2, "initial identify + one re-identify");
    let (s, _) = sink();
    let res = run_action_guarded(ip, cfg(MinerAction::Start, false), r.factory.clone(), CancellationToken::new(), s, ctx(&r, "3")).await;
    assert_eq!(res[0].status, ActionStatus::Verified, "{res:?}");
    assert!(!r.axe.paused());
    assert_eq!(r.factory.connects(), 2);
}

// ---------------- batch settings jobs ----------------

struct Fleet {
    miners: Vec<Arc<MockMiner>>,
    ips: Vec<IpAddr>,
    factory: Arc<MockMinerFactory>,
    cache: Arc<DeviceCache>,
    resolver: Arc<Resolver>,
    engine: Arc<SafetyEngine>,
}

async fn fleet(n: usize) -> Fleet {
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    let mut miners = Vec::new();
    let mut ips = Vec::new();
    for i in 0..n {
        let m = MockMiner::spawn(MockKind::AxeOs215, Duration::from_millis(600)).await.unwrap();
        let ip = IpAddr::from([198, 51, 100, 10 + i as u8]);
        f.add(ip, &m);
        miners.push(Arc::new(m));
        ips.push(ip);
    }
    let factory = Arc::new(f);
    let cache = Arc::new(DeviceCache::default());
    let resolver = Arc::new(Resolver::new(factory.clone(), cache.clone()));
    let engine = Arc::new(SafetyEngine::new(Arc::new(MemoryAudit::default()), Arc::new(SnapshotStore::in_memory(64)), opts(true)));
    Fleet { miners, ips, factory, cache, resolver, engine }
}

fn batch_opts(concurrency: usize, canary: Option<usize>) -> BatchOptions {
    BatchOptions {
        concurrency,
        stagger: Duration::from_millis(5),
        canary_size: canary,
        control_interval: Duration::from_millis(40),
        resolve_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}

async fn run_batch(fl: &Fleet, ips: Vec<IpAddr>, o: BatchOptions) -> (Vec<WriteOutcome>, Option<JobAlert>, Vec<String>) {
    let rows = Arc::new(Mutex::new(Vec::new()));
    let r2 = rows.clone();
    let sink: super::batch::OutcomeSink = Arc::new(move |o: &WriteOutcome| r2.lock().unwrap().push(o.ip.clone()));
    let (out, alert) =
        run_settings_job("job", ips, apply(freq(575)), fl.resolver.clone(), fl.engine.clone(), o, CancellationToken::new(), sink).await;
    let rows = rows.lock().unwrap().clone();
    (out, alert, rows)
}

/// Permanent: one aggregated alert per job, grouped by cause, with
/// per-miner before/after values.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_batch_alert_groups_rollbacks_by_cause() {
    let fl = fleet(4).await;
    fl.miners[0].set_fault(Some(Fault::Overheat));
    fl.miners[1].set_fault(Some(Fault::HashrateDrop));
    fl.miners[2].set_ignore_writes(true);
    let (out, alert, rows) = run_batch(&fl, fl.ips.clone(), batch_opts(4, None)).await;
    assert_eq!(rows.len(), 4, "every miner reported");
    assert_eq!(out[3].status, WriteStatus::Verified, "{:?}", out[3]);
    let a = alert.expect("one alert for the job");
    assert_eq!(a.headline, "3 of 4 rolled back");
    let causes: Vec<_> = a.groups.iter().map(|g| (g.cause, g.count)).collect();
    for c in [FailureCause::Overheat, FailureCause::HashrateDrop, FailureCause::ReadBackMismatch] {
        assert!(causes.contains(&(c, 1)), "{causes:?}");
    }
    for g in &a.groups {
        let m = &g.miners[0];
        assert_eq!(m.before, Some(freq(MOCK_FREQ_MHZ)));
        assert_eq!(m.attempted, Some(freq(575)));
        assert_eq!(m.after, Some(freq(MOCK_FREQ_MHZ)), "after = restored value");
        assert!(!m.likely_external);
    }
    assert_eq!(a.retry.ips.len(), 3);
    assert!(a.retry.dry_run);
    for m in &fl.miners[..3] {
        assert_eq!(m.settings(), HEALTHY);
    }
    assert_eq!(fl.miners[3].settings().0, 575);
}

/// Permanent: first-wave failures above the threshold halt the rest.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_canary_halts_remaining_miners() {
    let fl = fleet(7).await;
    fl.miners[0].set_fault(Some(Fault::HashrateDrop));
    fl.miners[1].set_fault(Some(Fault::HashrateDrop));
    let (out, alert, rows) = run_batch(&fl, fl.ips.clone(), batch_opts(5, Some(5))).await;
    assert_eq!(rows.len(), 7);
    for o in &out[5..] {
        assert_eq!(o.status, WriteStatus::Skipped, "{o:?}");
        assert_eq!(o.skip_reason.as_deref(), Some("halted"));
        assert!(o.msg.as_deref().unwrap().contains("halted: 2 of 5 failed in the first wave (40% > 20%)"), "{o:?}");
    }
    for m in &fl.miners[5..] {
        assert_eq!(m.writes(), 0, "halted miners are never written");
        assert!(!m.requests().iter().any(|q| q.starts_with("PATCH")));
    }
    let a = alert.unwrap();
    assert_eq!(a.headline, "2 of 7 rolled back, 2 halted");
    assert_eq!(a.halted_ips.len(), 2);
    assert_eq!(a.retry.ips.len(), 4, "failed + halted are offered for retry");
}

/// At the threshold (20% of the first wave) the job continues.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_canary_at_threshold_continues() {
    let fl = fleet(7).await;
    fl.miners[0].set_fault(Some(Fault::HashrateDrop));
    let (out, alert, _) = run_batch(&fl, fl.ips.clone(), batch_opts(5, Some(5))).await;
    assert!(out.iter().all(|o| o.status != WriteStatus::Skipped), "{out:?}");
    assert_eq!(out.iter().filter(|o| o.status == WriteStatus::Verified).count(), 6);
    assert_eq!(alert.unwrap().headline, "1 of 7 rolled back");
}

/// Permanent: failures that coincide with degradation on untouched miners
/// are flagged as likely external (network/pool).
#[tokio::test(flavor = "multi_thread")]
async fn e2e_external_outage_is_flagged() {
    let fl = fleet(4).await;
    let (job, controls) = fl.ips.split_at(2);
    for m in &fl.miners[..2] {
        m.set_fault(Some(Fault::HashrateDrop));
    }
    let mut o = batch_opts(2, None);
    o.controls = controls.to_vec();
    let ctl: Vec<Arc<MockMiner>> = fl.miners[2..].to_vec();
    let changed: Vec<Arc<MockMiner>> = fl.miners[..2].to_vec();
    let outage = tokio::spawn(async move {
        // Start the outage from events, not a fixed delay (a fixed 300 ms
        // flaked on a slow CI runner): once both job miners were written
        // (inside their watch windows) and the control sampler has polled
        // every control at least twice more (so its pre-outage baseline is
        // taken and the loop is running).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while changed.iter().any(|m| m.writes() == 0) {
            assert!(tokio::time::Instant::now() < deadline, "job miners never written");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let seen: Vec<usize> = ctl.iter().map(|m| m.requests().len()).collect();
        while ctl.iter().zip(&seen).any(|(m, n)| m.requests().len() < n + 2) {
            assert!(tokio::time::Instant::now() < deadline, "controls never sampled");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        for m in &ctl {
            m.set_outage(true);
        }
    });
    let (out, alert, _) = run_batch(&fl, job.to_vec(), o).await;
    outage.await.unwrap();
    assert!(out.iter().all(|o| o.status == WriteStatus::RolledBack && o.likely_external), "{out:?}");
    let a = alert.unwrap();
    assert!(a.likely_external);
    assert!(a.headline.ends_with("— likely external (network/pool)"), "{}", a.headline);
    let note = a.external_note.unwrap();
    assert!(note.contains("2 of 2 failures coincided") && note.contains("198.51.100.12"), "{note}");
    for m in &fl.miners[2..] {
        assert_eq!(m.writes(), 0, "control miners are only read");
    }
    let _ = fl.cache.len();
    let _ = fl.factory.connects();
}

/// Healthy control miners: failures stay attributed to the change.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_device_failures_not_flagged_external_when_controls_healthy() {
    let fl = fleet(4).await;
    let (job, controls) = fl.ips.split_at(2);
    fl.miners[0].set_fault(Some(Fault::HashrateDrop));
    let mut o = batch_opts(2, None);
    o.controls = controls.to_vec();
    let (out, alert, _) = run_batch(&fl, job.to_vec(), o).await;
    assert_eq!(out[0].status, WriteStatus::RolledBack);
    assert!(!out[0].likely_external);
    let a = alert.unwrap();
    assert!(!a.likely_external && a.external_note.is_none());
    assert_eq!(a.headline, "1 of 2 rolled back");
}

/// Permanent: a 2.15 Bitaxe rebuilt from the discovery cache keeps native
/// (asic-rs) pause/resume, reboot on the AxeOS override, and AxeOS
/// identity / health / settings, with no identification.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_cached_bitaxe_215_keeps_native_route_and_axeos_identity() {
    use crate::control::cache::DriverKind;
    use crate::control::driver::Route;
    let r = rig().await;
    r.cache.put(IpAddr::from(AXE), r.factory.discovered(IpAddr::from(AXE)).unwrap());
    let t = target(&r, AXE).await;
    let caps = t.driver.caps();
    assert_eq!(caps.route(MinerAction::Stop), Some(Route::AsicRs));
    assert_eq!(caps.route(MinerAction::Start), Some(Route::AsicRs));
    assert_eq!(caps.route(MinerAction::Reboot), Some(Route::Axeos));
    assert_eq!(t.driver.kind(), DriverKind::AxeOs, "cache rebuilds the same composite");
    assert_eq!(t.profile.class, DeviceClass::AxeOs);
    assert!(t.driver.health().await.hashrate_ghs.is_some(), "health from AxeOS info");
    assert_eq!(r.factory.connects(), 0);
    let o = r.engine.apply_settings("run", &t, &apply(freq(575))).await;
    assert_eq!(o.status, WriteStatus::Verified, "{o:?}");
    assert_eq!(o.paths_tried, ["axeos-http: selected"]);
}
