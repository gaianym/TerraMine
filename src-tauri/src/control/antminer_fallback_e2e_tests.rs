//! Stock Antminer fallbacks (plan PR K1), end to end: job runner → real
//! TCP / HTTP drivers → the stateful S9 mock (digest CGI + bmminer RPC).
//!
//! - CGI login rejected → bmminer RPC `restart` as the reboot path:
//!   capability-checked (`privileged`), dry run only until confirmed on a
//!   real S9, verified by the mining uptime resetting.
//! - Start / stop / light with no CGI route list the RPC path too.
//! - The allowlist means no config-rewriting or pool command ever reaches
//!   the miner, and the whole-config CGI is never used.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::driver::{DriverFactory, MinerAction, Route};
use super::job::{run_action, ActionRowEvent, ActionStatus, JobConfig, Sink, VerifyConfig};
use super::mock_miner::{MockKind, MockMiner, MockMinerFactory};
use super::overrides::DigestCredentials;

const S9: [u8; 4] = [192, 0, 2, 60];
const AXE: [u8; 4] = [192, 0, 2, 61];

struct Rig {
    s9: MockMiner,
    axe: MockMiner,
    factory: Arc<MockMinerFactory>,
}

/// `bad_login`: TerraMine's digest credentials are wrong for this S9.
async fn rig(bad_login: bool) -> Rig {
    let down = Duration::from_millis(600);
    let s9 = MockMiner::spawn(MockKind::S9Stock { miner_mode: false }, down).await.unwrap();
    let axe = MockMiner::spawn(MockKind::AxeOs215, down).await.unwrap();
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    f.add(IpAddr::from(S9), &s9);
    f.add(IpAddr::from(AXE), &axe);
    if bad_login {
        f.set_creds(IpAddr::from(S9), DigestCredentials { username: "root".into(), password: "wrong".into() });
    }
    Rig { s9, axe, factory: Arc::new(f) }
}

fn cfg(action: MinerAction, dry_run: bool, allow_unconfirmed: bool) -> JobConfig {
    JobConfig {
        action,
        concurrency: 4,
        stagger: Duration::from_millis(10),
        timeout: Duration::from_secs(2),
        dry_run,
        verify: Some(VerifyConfig { interval: Duration::from_millis(200), timeout: Duration::from_secs(10) }),
        allow_unconfirmed,
    }
}

fn sink() -> (Sink, Arc<Mutex<Vec<ActionRowEvent>>>) {
    let v = Arc::new(Mutex::new(Vec::new()));
    let v2 = v.clone();
    (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
}

async fn run(r: &Rig, ip: [u8; 4], c: JobConfig) -> ActionRowEvent {
    let (s, _) = sink();
    let out = run_action(vec![IpAddr::from(ip)], c, r.factory.clone(), CancellationToken::new(), s).await;
    out.into_iter().next().unwrap()
}

/// Nothing that rewrites config or touches pools ever reached the S9.
fn assert_no_config_writes(m: &MockMiner) {
    for q in m.requests() {
        assert!(!q.contains("set_miner_conf"), "whole-config CGI used: {q}");
        for c in ["save", "addpool", "removepool", "switchpool", "enablepool", "disablepool", "ascset"] {
            assert_ne!(q, format!("cgminer {c}"), "forbidden RPC sent");
        }
    }
}

/// Login rejected: reboot falls back to bmminer `restart`; dry run by
/// default shows the route and sends nothing; a live send is refused
/// until the route is confirmed on a real S9.
#[tokio::test(flavor = "multi_thread")]
async fn rpc_restart_fallback_is_dry_run_only_until_confirmed() {
    let r = rig(true).await;
    let d = r.factory.connect(IpAddr::from(S9)).await.unwrap().expect("RPC fallback driver");
    let caps = d.caps();
    assert_eq!(caps.route(MinerAction::Reboot), Some(Route::CgminerRpc), "{caps:?}");
    assert!(!caps.confirmed(MinerAction::Reboot), "RPC restart is not hardware-confirmed");
    assert_eq!(caps.as_flags(), (false, false, true));

    let e = run(&r, S9, cfg(MinerAction::Reboot, true, false)).await;
    assert_eq!(e.status, ActionStatus::DryRun, "{e:?}");
    assert_eq!(e.route, Some(Route::CgminerRpc));
    assert!(e.msg.as_deref().unwrap().contains("would send via cgminer-rpc; route not yet confirmed on real hardware"), "{e:?}");

    let e = run(&r, S9, cfg(MinerAction::Reboot, false, false)).await;
    assert_eq!(e.status, ActionStatus::Refused, "{e:?}");
    assert!(e.msg.as_deref().unwrap().contains("reboot via cgminer-rpc is not yet confirmed on real hardware"), "{e:?}");

    assert_eq!(r.s9.mining_restarts(), 0);
    assert_eq!(r.s9.reboots(), 0);
    assert!(!r.s9.requests().iter().any(|q| q == "cgminer restart"), "dry run / refusal sent restart");
    assert!(r.s9.requests().iter().any(|q| q == "cgminer privileged"), "capability checked first");
    assert_no_config_writes(&r.s9);
}

/// Once allowed (what a hardware confirmation unlocks), the restart is
/// sent once and verified by bmminer's uptime resetting; the controller
/// itself does not reboot.
#[tokio::test(flavor = "multi_thread")]
async fn rpc_restart_verified_by_uptime_reset() {
    let r = rig(true).await;
    let e = run(&r, S9, cfg(MinerAction::Reboot, false, true)).await;
    assert_eq!(e.status, ActionStatus::Verified, "{e:?}");
    assert_eq!(e.route, Some(Route::CgminerRpc));
    assert_eq!(r.s9.mining_restarts(), 1);
    assert_eq!(r.s9.reboots(), 0, "bmminer restart, not a controller reboot");
    assert_eq!(r.s9.requests().iter().filter(|q| *q == "cgminer restart").count(), 1);
    assert_no_config_writes(&r.s9);
}

/// Privileged access denied: no route; "not supported" lists the CGI and
/// the RPC reasons; restart never sent.
#[tokio::test(flavor = "multi_thread")]
async fn rpc_denied_reports_every_path_tried() {
    let r = rig(true).await;
    r.s9.set_rpc_denied(true);
    let e = run(&r, S9, cfg(MinerAction::Reboot, false, true)).await;
    assert_eq!(e.status, ActionStatus::Unsupported, "{e:?}");
    let msg = e.msg.as_deref().unwrap();
    assert!(msg.contains("s9-cgi: digest login rejected or CGI unavailable"), "{msg}");
    assert!(msg.contains("cgminer-rpc: privileged access denied"), "{msg}");
    assert!(!r.s9.requests().iter().any(|q| q == "cgminer restart"));
    assert_eq!(r.s9.mining_restarts() + r.s9.reboots(), 0);
}

/// With a working login the CGI stays the reboot path (confirmed on
/// hardware); stop / start / light list both the CGI and RPC paths.
#[tokio::test(flavor = "multi_thread")]
async fn cgi_stays_primary_and_gaps_list_both_paths() {
    let r = rig(false).await;
    let caps = r.factory.connect(IpAddr::from(S9)).await.unwrap().unwrap().caps();
    assert_eq!(caps.route(MinerAction::Reboot), Some(Route::S9Cgi));
    for a in [MinerAction::Stop, MinerAction::Start] {
        let e = run(&r, S9, cfg(a, false, true)).await;
        assert_eq!(e.status, ActionStatus::Unsupported, "{e:?}");
        let msg = e.msg.as_deref().unwrap();
        assert!(msg.contains("s9-cgi: no miner-mode on this firmware"), "{msg}");
        assert!(msg.contains("cgminer-rpc: bmminer has no pause / resume command"), "{msg}");
    }
    let r2 = rig(true).await;
    let e = run(&r2, S9, cfg(MinerAction::LightOn, false, true)).await;
    assert_eq!(e.status, ActionStatus::Unsupported, "{e:?}");
    assert!(e.msg.as_deref().unwrap().contains("cgminer-rpc: bmminer has no locate-light command"), "{e:?}");
    assert!(!r.s9.paused());
    assert_eq!(r.s9.mining_restarts() + r2.s9.mining_restarts(), 0);
    assert_no_config_writes(&r.s9);
    assert_no_config_writes(&r2.s9);
}

/// S9 health (used by the post-change watch) now carries the hottest chip
/// temperature from bmminer `stats`, read-only.
#[tokio::test(flavor = "multi_thread")]
async fn s9_health_reads_chip_temperature() {
    for bad_login in [false, true] {
        let r = rig(bad_login).await;
        let d = r.factory.connect(IpAddr::from(S9)).await.unwrap().unwrap();
        let h = d.health().await;
        assert!(h.reachable);
        assert_eq!(h.hashrate_ghs, Some(13500.0));
        assert_eq!(h.temp_c, Some(75.0), "bad_login={bad_login}");
        assert_no_config_writes(&r.s9);
    }
}

/// The Bitaxe never receives a cgminer request, whatever is run on it.
#[tokio::test(flavor = "multi_thread")]
async fn bitaxe_never_gets_cgminer_rpc() {
    let r = rig(true).await;
    for a in MinerAction::ALL {
        let _ = run(&r, AXE, cfg(a, true, false)).await;
    }
    let d = r.factory.connect(IpAddr::from(AXE)).await.unwrap().unwrap();
    let _ = d.health().await;
    assert!(r.axe.cgminer.is_none());
    assert!(!r.axe.requests().iter().any(|q| q.starts_with("cgminer")), "{:?}", r.axe.requests());
    assert!(!d.paths_tried(MinerAction::Reboot).iter().any(|p| p.starts_with("cgminer-rpc")));
}

/// Permanent: every backend route has a matching UI `Route` type, and the
/// serialized name is the label (a new route cannot silently show as an
/// unknown string in the app).
#[test]
fn every_route_is_known_to_the_ui() {
    // Exhaustive on purpose: adding a variant fails to compile here.
    fn all() -> [Route; 5] {
        match Route::Mock {
            Route::AsicRs | Route::Axeos | Route::S9Cgi | Route::CgminerRpc | Route::Mock => {}
        }
        [Route::AsicRs, Route::Axeos, Route::S9Cgi, Route::CgminerRpc, Route::Mock]
    }
    let ts = include_str!("../../../src/lib/control/types.ts");
    let line = ts.lines().find(|l| l.starts_with("export type Route =")).expect("Route type in types.ts");
    for r in all() {
        assert_eq!(serde_json::to_value(r).unwrap(), serde_json::json!(r.label()));
        assert!(line.contains(&format!("\"{}\"", r.label())), "{} missing from types.ts: {line}", r.label());
    }
}
