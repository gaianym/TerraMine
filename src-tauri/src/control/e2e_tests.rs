//! End-to-end: job runner → real HTTP/TCP drivers → stateful mock miners.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::driver::{DriverFactory, MinerAction, Route};
use super::job::{run_action, ActionRowEvent, ActionStatus, JobConfig, Sink, VerifyConfig};
use super::mock_miner::{MockKind, MockMiner, MockMinerFactory};

const AXE210: [u8; 4] = [192, 0, 2, 1];
const AXE215: [u8; 4] = [192, 0, 2, 2];
const S9: [u8; 4] = [192, 0, 2, 3];
const S9_MODE: [u8; 4] = [192, 0, 2, 4];

struct Fleet {
    axe210: MockMiner,
    axe215: MockMiner,
    s9: MockMiner,
    s9_mode: MockMiner,
    factory: Arc<MockMinerFactory>,
}

async fn fleet() -> Fleet {
    let down = Duration::from_millis(600);
    let axe210 = MockMiner::spawn(MockKind::AxeOs210, down).await.unwrap();
    let axe215 = MockMiner::spawn(MockKind::AxeOs215, down).await.unwrap();
    let s9 = MockMiner::spawn(MockKind::S9Stock { miner_mode: false }, down).await.unwrap();
    let s9_mode = MockMiner::spawn(MockKind::S9Stock { miner_mode: true }, down).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    f.add(IpAddr::from(AXE210), &axe210);
    f.add(IpAddr::from(AXE215), &axe215);
    f.add(IpAddr::from(S9), &s9);
    f.add(IpAddr::from(S9_MODE), &s9_mode);
    Fleet { axe210, axe215, s9, s9_mode, factory: Arc::new(f) }
}

fn cfg(action: MinerAction) -> JobConfig {
    JobConfig {
        action,
        concurrency: 4,
        stagger: Duration::from_millis(10),
        timeout: Duration::from_secs(2),
        dry_run: false,
        verify: Some(VerifyConfig { interval: Duration::from_millis(200), timeout: Duration::from_secs(10) }),
    }
}

fn sink() -> (Sink, Arc<Mutex<Vec<ActionRowEvent>>>) {
    let v = Arc::new(Mutex::new(Vec::new()));
    let v2 = v.clone();
    (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
}

fn all_ips() -> Vec<IpAddr> {
    [AXE210, AXE215, S9, S9_MODE].into_iter().map(IpAddr::from).collect()
}

fn by_ip(r: &[ActionRowEvent], ip: [u8; 4]) -> ActionRowEvent {
    let ip = IpAddr::from(ip).to_string();
    r.iter().find(|e| e.ip == ip).cloned().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_reboot_all_variants_verified_by_uptime() {
    let f = fleet().await;
    let (s, events) = sink();
    let r = run_action(all_ips(), cfg(MinerAction::Reboot), f.factory.clone(), CancellationToken::new(), s).await;
    for ip in [AXE210, AXE215, S9, S9_MODE] {
        assert_eq!(by_ip(&r, ip).status, ActionStatus::Verified, "{:?}", by_ip(&r, ip));
    }
    assert_eq!((f.axe210.reboots(), f.axe215.reboots(), f.s9.reboots(), f.s9_mode.reboots()), (1, 1, 1, 1));
    let ev = events.lock().unwrap();
    let seq: Vec<_> = ev.iter().filter(|e| e.ip == "192.0.2.3").map(|e| e.status).collect();
    assert_eq!(seq, vec![ActionStatus::Queued, ActionStatus::Sent, ActionStatus::Verifying, ActionStatus::Verified]);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_stop_then_start_verified_where_supported() {
    let f = fleet().await;
    let (s, _) = sink();
    let r = run_action(all_ips(), cfg(MinerAction::Stop), f.factory.clone(), CancellationToken::new(), s.clone()).await;
    assert_eq!(by_ip(&r, AXE210).status, ActionStatus::Unsupported);
    assert_eq!(by_ip(&r, S9).status, ActionStatus::Unsupported);
    assert_eq!(by_ip(&r, AXE215).status, ActionStatus::Verified, "{:?}", by_ip(&r, AXE215));
    assert_eq!(by_ip(&r, S9_MODE).status, ActionStatus::Verified, "{:?}", by_ip(&r, S9_MODE));
    assert!(f.axe215.paused() && f.s9_mode.paused());

    let r = run_action(all_ips(), cfg(MinerAction::Start), f.factory.clone(), CancellationToken::new(), s).await;
    assert_eq!(by_ip(&r, AXE215).status, ActionStatus::Verified);
    assert_eq!(by_ip(&r, S9_MODE).status, ActionStatus::Verified);
    assert!(!f.axe215.paused() && !f.s9_mode.paused());
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_dry_run_touches_nothing() {
    let f = fleet().await;
    let (s, _) = sink();
    let mut c = cfg(MinerAction::Reboot);
    c.dry_run = true;
    let r = run_action(all_ips(), c, f.factory.clone(), CancellationToken::new(), s).await;
    assert!(r.iter().all(|e| e.status == ActionStatus::DryRun));
    assert_eq!(f.axe210.reboots() + f.s9.reboots(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_reboot_unverified_when_miner_never_returns() {
    let m = MockMiner::spawn(MockKind::AxeOs215, Duration::from_secs(30)).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(1));
    f.add(IpAddr::from(AXE215), &m);
    let mut c = cfg(MinerAction::Reboot);
    c.verify = Some(VerifyConfig { interval: Duration::from_millis(100), timeout: Duration::from_millis(800) });
    let (s, _) = sink();
    let r = run_action(vec![IpAddr::from(AXE215)], c, Arc::new(f), CancellationToken::new(), s).await;
    assert_eq!(r[0].status, ActionStatus::Unverified);
    assert!(r[0].msg.as_deref().unwrap_or("").contains("not confirmed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_s9_wrong_password_never_reboots() {
    let m = MockMiner::spawn(MockKind::S9Stock { miner_mode: false }, Duration::from_millis(100)).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(1));
    f.add(IpAddr::from(S9), &m);
    f.miners.get_mut(&IpAddr::from(S9)).unwrap().3.password = "nope".into();
    let (s, _) = sink();
    let r = run_action(vec![IpAddr::from(S9)], cfg(MinerAction::Reboot), Arc::new(f), CancellationToken::new(), s).await;
    assert!(matches!(r[0].status, ActionStatus::NotFound | ActionStatus::Failed), "{:?}", r[0]);
    assert_eq!(m.reboots(), 0);
}

// ---------------- capability map (asic-rs 0.8.6) ----------------

fn final_for(r: &[ActionRowEvent], ip: [u8; 4]) -> ActionRowEvent {
    by_ip(r, ip)
}

/// Permanent regression: fleet Bitaxes on AxeOS 2.15.3 use the asic-rs
/// `v2_14_0` backend natively for pause/resume (no longer reboot-only);
/// reboot, which asic-rs does not support there, stays on the AxeOS
/// override.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_axeos_215_pause_resume_native_reboot_via_override() {
    let f = fleet().await;
    let caps = f.factory.connect(IpAddr::from(AXE215)).await.unwrap().unwrap().caps();
    assert_eq!(caps.route(MinerAction::Stop), Some(Route::AsicRs));
    assert_eq!(caps.route(MinerAction::Start), Some(Route::AsicRs));
    assert_eq!(caps.route(MinerAction::Reboot), Some(Route::Axeos));

    for (action, paused) in [(MinerAction::Stop, true), (MinerAction::Start, false)] {
        let (s, events) = sink();
        let r = run_action(vec![IpAddr::from(AXE215)], cfg(action), f.factory.clone(), CancellationToken::new(), s).await;
        let e = final_for(&r, AXE215);
        assert_eq!(e.status, ActionStatus::Verified, "{action:?}: {e:?}");
        assert_eq!(e.route, Some(Route::AsicRs), "{action:?} sent natively");
        assert!(events.lock().unwrap().iter().filter(|e| e.status == ActionStatus::Sent).all(|e| e.route == Some(Route::AsicRs)));
        assert_eq!(f.axe215.paused(), paused);
    }
    let (s, _) = sink();
    let r = run_action(vec![IpAddr::from(AXE215)], cfg(MinerAction::Reboot), f.factory.clone(), CancellationToken::new(), s).await;
    let e = final_for(&r, AXE215);
    assert_eq!(e.status, ActionStatus::Verified, "{e:?}");
    assert_eq!(e.route, Some(Route::Axeos), "reboot stays on the override");
    assert_eq!(f.axe215.reboots(), 1);
}

/// AxeOS 2.10 has no asic-rs native backend for pause: still reboot-only,
/// and stop/start are refused before anything is sent.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_axeos_210_stays_reboot_only_via_override() {
    let f = fleet().await;
    for action in [MinerAction::Stop, MinerAction::Start] {
        let (s, _) = sink();
        let r = run_action(vec![IpAddr::from(AXE210)], cfg(action), f.factory.clone(), CancellationToken::new(), s).await;
        let e = final_for(&r, AXE210);
        assert_eq!(e.status, ActionStatus::Unsupported, "{e:?}");
        assert_eq!(e.route, None);
    }
    assert!(!f.axe210.paused());
    let (s, _) = sink();
    let r = run_action(vec![IpAddr::from(AXE210)], cfg(MinerAction::Reboot), f.factory.clone(), CancellationToken::new(), s).await;
    assert_eq!(final_for(&r, AXE210).route, Some(Route::Axeos));
    assert_eq!(f.axe210.reboots(), 1);
}

/// Stock S9 keeps its CGI override for every action.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_s9_routes_through_cgi_override() {
    let f = fleet().await;
    let caps = f.factory.connect(IpAddr::from(S9_MODE)).await.unwrap().unwrap().caps();
    assert!(MinerAction::ALL.iter().all(|a| caps.route(*a) == Some(Route::S9Cgi)), "{caps:?}");
    let caps = f.factory.connect(IpAddr::from(S9)).await.unwrap().unwrap().caps();
    assert_eq!(caps.as_flags(), (false, false, true));
    let (s, _) = sink();
    let r = run_action(vec![IpAddr::from(S9_MODE)], cfg(MinerAction::Stop), f.factory.clone(), CancellationToken::new(), s).await;
    assert_eq!(final_for(&r, S9_MODE).route, Some(Route::S9Cgi));
    assert!(f.s9_mode.paused());
}
