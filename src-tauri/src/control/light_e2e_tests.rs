//! Fault light end to end: job runner → capability gate → real HTTP
//! drivers → stateful mocks, across every device class we model.
//!
//! - asic-rs backends with `supports_set_fault_light = true` (Antminer
//!   stock v2020, Braiins 25.07, Whatsminer v3) are sent and read back.
//! - Whatsminer v1 and AxeOS 2.10 have no path: `unsupported`, with every
//!   path tried and why.
//! - AxeOS `identify` is hardware-confirmed on 2.15.x: sent live once,
//!   with nothing to read back (`unverified`).
//! - Fallbacks not yet confirmed on hardware (stock S9 `blink.cgi`, AxeOS
//!   `identify` outside 2.15.x) are dry-run only: a live run refuses them
//!   and sends nothing.
//! - Each fallback only ever reaches its own device class.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::driver::{DriverFactory, MinerAction, Route};
use super::job::{run_action, run_action_guarded, ActionRowEvent, ActionStatus, JobConfig, SafetyCtx, Sink, VerifyConfig};
use super::mock_miner::{MockFirmware, MockKind, MockMiner, MockMinerFactory};
use super::safety::MemoryAudit;

const AXE210: [u8; 4] = [192, 0, 2, 21];
const AXE215: [u8; 4] = [192, 0, 2, 22];
const S9: [u8; 4] = [192, 0, 2, 23];
const ANTMINER: [u8; 4] = [192, 0, 2, 24];
const BRAIINS: [u8; 4] = [192, 0, 2, 25];
const WM3: [u8; 4] = [192, 0, 2, 26];
const WM1: [u8; 4] = [192, 0, 2, 27];

const ALL: [[u8; 4]; 7] = [AXE210, AXE215, S9, ANTMINER, BRAIINS, WM3, WM1];
const NATIVE_LIGHT: [[u8; 4]; 3] = [ANTMINER, BRAIINS, WM3];

struct Fleet {
    miners: HashMap<[u8; 4], MockMiner>,
    factory: Arc<MockMinerFactory>,
}

impl Fleet {
    fn m(&self, ip: [u8; 4]) -> &MockMiner {
        &self.miners[&ip]
    }
    /// Requests on `ip` whose "METHOD /path" contains `needle`.
    fn hits(&self, ip: [u8; 4], needle: &str) -> usize {
        self.m(ip).requests().iter().filter(|r| r.contains(needle)).count()
    }
}

async fn fleet() -> Fleet {
    let down = Duration::from_millis(300);
    let kinds = [
        (AXE210, MockKind::AxeOs210),
        (AXE215, MockKind::AxeOs215),
        (S9, MockKind::S9Stock { miner_mode: false }),
        (ANTMINER, MockKind::AsicRs(MockFirmware::AntminerV2020)),
        (BRAIINS, MockKind::AsicRs(MockFirmware::Braiins2507)),
        (WM3, MockKind::AsicRs(MockFirmware::WhatsminerV3)),
        (WM1, MockKind::AsicRs(MockFirmware::WhatsminerV1)),
    ];
    let mut f = MockMinerFactory::new(Duration::from_secs(2));
    let mut miners = HashMap::new();
    for (ip, k) in kinds {
        let m = MockMiner::spawn(k, down).await.unwrap();
        f.add(IpAddr::from(ip), &m);
        miners.insert(ip, m);
    }
    Fleet { miners, factory: Arc::new(f) }
}

fn cfg(action: MinerAction, dry_run: bool, allow_unconfirmed: bool) -> JobConfig {
    JobConfig {
        action,
        concurrency: 8,
        stagger: Duration::from_millis(5),
        timeout: Duration::from_secs(2),
        dry_run,
        verify: Some(VerifyConfig { interval: Duration::from_millis(100), timeout: Duration::from_secs(5) }),
        allow_unconfirmed,
    }
}

fn sink() -> (Sink, Arc<Mutex<Vec<ActionRowEvent>>>) {
    let v = Arc::new(Mutex::new(Vec::new()));
    let v2 = v.clone();
    (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
}

fn ips(list: &[[u8; 4]]) -> Vec<IpAddr> {
    list.iter().map(|a| IpAddr::from(*a)).collect()
}

async fn run(f: &Fleet, list: &[[u8; 4]], c: JobConfig) -> HashMap<[u8; 4], ActionRowEvent> {
    let (s, _) = sink();
    let out = run_action(ips(list), c, f.factory.clone() as Arc<dyn DriverFactory>, CancellationToken::new(), s).await;
    list.iter().copied().zip(out).collect()
}

fn msg(e: &ActionRowEvent) -> &str {
    e.msg.as_deref().unwrap_or("")
}

#[tokio::test]
async fn light_fleetwide_live_routes_per_class() {
    let f = fleet().await;
    let audit = Arc::new(MemoryAudit::default());
    let ctx = SafetyCtx { run_id: "light-run".into(), audit: audit.clone(), ..Default::default() };
    let (s, _) = sink();
    let out = run_action_guarded(
        ips(&ALL),
        cfg(MinerAction::LightOn, false, false),
        f.factory.clone() as Arc<dyn DriverFactory>,
        CancellationToken::new(),
        s,
        ctx,
    )
    .await;
    let r: HashMap<[u8; 4], ActionRowEvent> = ALL.iter().copied().zip(out).collect();

    for ip in NATIVE_LIGHT {
        assert_eq!(r[&ip].status, ActionStatus::Verified, "{ip:?}: {:?}", r[&ip]);
        assert_eq!(r[&ip].route, Some(Route::AsicRs));
        assert!(f.m(ip).light(), "{ip:?} light on");
        assert_eq!(f.hits(ip, "POST /asic-rs/fault_light"), 1, "{ip:?}: exactly one send");
    }
    // No path at all: unsupported with every path tried and why.
    assert_eq!(r[&WM1].status, ActionStatus::Unsupported);
    assert!(msg(&r[&WM1]).contains("paths tried") && msg(&r[&WM1]).contains("supports_set_fault_light() = false"), "{}", msg(&r[&WM1]));
    assert_eq!(f.hits(WM1, "POST"), 0);
    assert_eq!(r[&AXE210].status, ActionStatus::Unsupported);
    assert!(msg(&r[&AXE210]).contains("no identify endpoint before AxeOS 2.12"), "{}", msg(&r[&AXE210]));
    assert_eq!(f.hits(AXE210, "POST"), 0);
    // AxeOS 2.15.x identify is hardware-confirmed: sent live exactly once
    // (a second call would toggle), nothing to read back.
    assert_eq!(r[&AXE215].status, ActionStatus::Unverified, "{:?}", r[&AXE215]);
    assert_eq!(r[&AXE215].route, Some(Route::Axeos));
    assert!(msg(&r[&AXE215]).contains("does not report the light state"), "{}", msg(&r[&AXE215]));
    assert_eq!(f.hits(AXE215, "POST /api/system/identify"), 1);
    assert!(f.m(AXE215).identifying());
    // Unconfirmed fallback: refused live, nothing sent.
    assert_eq!(r[&S9].status, ActionStatus::Refused, "{:?}", r[&S9]);
    assert_eq!(r[&S9].route, Some(Route::S9Cgi));
    assert!(msg(&r[&S9]).contains("not yet confirmed on real hardware"), "{}", msg(&r[&S9]));
    assert_eq!(f.hits(S9, "blink"), 0);
    assert!(!f.m(S9).light());

    // Audit: one attempt per send and one outcome per miner, under the light action name.
    let recs = audit.records();
    assert!(recs.iter().all(|r| r.action == "light_on" && r.run_id == "light-run"));
    assert_eq!(recs.iter().filter(|r| r.phase == "attempt").count(), NATIVE_LIGHT.len() + 1);
    assert_eq!(recs.iter().filter(|r| r.phase == "outcome").count(), ALL.len());

    // And off again, read back on the same path.
    let r = run(&f, &NATIVE_LIGHT, cfg(MinerAction::LightOff, false, false)).await;
    for ip in NATIVE_LIGHT {
        assert_eq!(r[&ip].status, ActionStatus::Verified, "{ip:?}: {:?}", r[&ip]);
        assert!(!f.m(ip).light());
    }
}

#[tokio::test]
async fn light_dry_run_is_the_default_and_sends_nothing() {
    let c = JobConfig::new(MinerAction::LightOn);
    assert!(c.dry_run, "fault light is a new action class: dry run by default");
    assert!(!JobConfig::new(MinerAction::Reboot).dry_run, "phase-1 default unchanged");
    assert!(!c.allow_unconfirmed);
    let f = fleet().await;
    let r = run(&f, &ALL, cfg(MinerAction::LightOn, true, false)).await;
    for ip in [AXE215, S9, ANTMINER, BRAIINS, WM3] {
        assert_eq!(r[&ip].status, ActionStatus::DryRun, "{ip:?}: {:?}", r[&ip]);
    }
    assert!(msg(&r[&S9]).contains("not yet confirmed"), "dry run flags unconfirmed routes: {}", msg(&r[&S9]));
    assert_eq!(r[&ANTMINER].msg, None);
    assert_eq!(r[&WM1].status, ActionStatus::Unsupported);
    for ip in ALL {
        assert_eq!(f.hits(ip, "POST"), 0, "{ip:?}: dry run must not write");
        assert!(!f.m(ip).light());
    }
}

/// With the hardware-confirmation gate lifted (mock only), each fallback
/// works on its own class and never reaches another class.
#[tokio::test]
async fn light_fallbacks_on_mocks_stay_in_their_class() {
    let f = fleet().await;
    let r = run(&f, &ALL, cfg(MinerAction::LightOn, false, true)).await;
    assert_eq!(r[&S9].status, ActionStatus::Verified, "{:?}", r[&S9]);
    assert_eq!(r[&S9].route, Some(Route::S9Cgi));
    assert!(f.m(S9).light());
    // Digest auth: the unauthenticated challenge plus the one authenticated send.
    assert_eq!(f.hits(S9, "POST /cgi-bin/blink.cgi"), 2);
    // AxeOS identify: sent once, but nothing to read back.
    assert_eq!(r[&AXE215].status, ActionStatus::Unverified, "{:?}", r[&AXE215]);
    assert_eq!(r[&AXE215].route, Some(Route::Axeos));
    assert!(msg(&r[&AXE215]).contains("does not report the light state"));
    assert!(f.m(AXE215).identifying());
    assert_eq!(f.hits(AXE215, "POST /api/system/identify"), 1);
    // Class gating: AxeOS paths only on AxeOS, CGI only on the S9, asic-rs paths only on asic-rs devices.
    for ip in ALL {
        if ip != AXE215 && ip != AXE210 {
            assert_eq!(f.hits(ip, "/api/system"), 0, "{ip:?} got an AxeOS call");
        }
        if ip != S9 {
            assert_eq!(f.hits(ip, "/cgi-bin/"), 0, "{ip:?} got an Antminer CGI call");
        }
        if !NATIVE_LIGHT.contains(&ip) && ip != WM1 {
            assert_eq!(f.hits(ip, "/asic-rs/"), 0, "{ip:?} got an asic-rs call");
        }
    }
    // Off: the S9 reads back off; AxeOS has no off command and says so.
    let r = run(&f, &[S9, AXE215], cfg(MinerAction::LightOff, false, true)).await;
    assert_eq!(r[&S9].status, ActionStatus::Verified);
    assert!(!f.m(S9).light());
    assert_eq!(r[&AXE215].status, ActionStatus::Unsupported);
    assert!(msg(&r[&AXE215]).contains("no off command"), "{}", msg(&r[&AXE215]));
    assert_eq!(f.hits(AXE215, "POST /api/system/identify"), 1, "no second (toggling) identify");
}

#[tokio::test]
async fn caps_rows_explain_unsupported_light() {
    let f = fleet().await;
    let get = |ip: [u8; 4]| {
        let f = f.factory.clone();
        async move { f.connect(IpAddr::from(ip)).await.unwrap().unwrap() }
    };
    let wm1 = get(WM1).await;
    let row = super::caps_row(IpAddr::from(WM1), Some(wm1.as_ref()));
    assert!(row.found && !row.caps.supports(MinerAction::LightOn));
    assert!(row.why[&MinerAction::LightOn].contains("supports_set_fault_light() = false"), "{:?}", row.why);
    let axe = get(AXE215).await;
    let row = super::caps_row(IpAddr::from(AXE215), Some(axe.as_ref()));
    // Composite: native first (asic-rs says no), then the AxeOS override
    // (identify, hardware-confirmed on 2.15.x).
    assert_eq!(row.caps.route(MinerAction::LightOn), Some(Route::Axeos));
    assert!(row.caps.confirmed(MinerAction::LightOn));
    assert!(!row.why.contains_key(&MinerAction::LightOn));
    let off = &row.why[&MinerAction::LightOff];
    assert!(off.contains("asic-rs: supports_set_fault_light() = false") && off.contains("no off command"), "{off}");
    let v = serde_json::to_value(&row).unwrap();
    assert_eq!(v["caps"]["light_on"], true);
    assert_eq!(v["caps"]["unconfirmed"], serde_json::json!([]));
    let none = super::caps_row(IpAddr::from(WM1), None);
    assert!(!none.found && none.why.is_empty());
}

#[test]
fn request_without_dry_run_follows_the_action_class() {
    let req: super::ActionRequest = serde_json::from_value(serde_json::json!({"ips": ["192.0.2.9"], "action": "light_on"})).unwrap();
    assert!(super::job_config(&req).dry_run);
    let req: super::ActionRequest = serde_json::from_value(serde_json::json!({"ips": ["192.0.2.9"], "action": "reboot"})).unwrap();
    assert!(!super::job_config(&req).dry_run);
    let req: super::ActionRequest =
        serde_json::from_value(serde_json::json!({"ips": ["192.0.2.9"], "action": "light_off", "dry_run": false})).unwrap();
    let c = super::job_config(&req);
    assert!(!c.dry_run && !c.allow_unconfirmed, "the app never lifts the hardware gate");
}

#[test]
fn blink_status_parsing() {
    use super::overrides::s9::parse_blink_status;
    use serde_json::json;
    assert_eq!(parse_blink_status(&json!({"blink": true})), Some(true));
    assert_eq!(parse_blink_status(&json!({"blink": "false"})), Some(false));
    assert_eq!(parse_blink_status(&json!({"blink": "1"})), Some(true));
    assert_eq!(parse_blink_status(&json!({"blink": "maybe"})), None);
    assert_eq!(parse_blink_status(&json!({})), None);
}

#[test]
fn axeos_identify_only_from_2_12() {
    use super::overrides::bitaxe::caps_for_version_str;
    assert!(!caps_for_version_str("v2.11.0").supports(MinerAction::LightOn));
    let c = caps_for_version_str("v2.12.0");
    assert_eq!(c.route(MinerAction::LightOn), Some(Route::Axeos));
    assert!(!c.confirmed(MinerAction::LightOn), "identify is dry-run only until a hardware check");
    assert!(!caps_for_version_str("v2.15.3").supports(MinerAction::LightOff));
}

/// Identify was sent live on a Bitaxe running v2.15.3 (hw_bitaxe_identify):
/// only the 2.15.x line is confirmed; others stay dry-run only.
#[test]
fn axeos_identify_hardware_confirmed_on_2_15_only() {
    use super::overrides::bitaxe::caps_for_version_str;
    for v in ["v2.15.0", "v2.15.3", "2.15.9"] {
        let c = caps_for_version_str(v);
        assert_eq!(c.route(MinerAction::LightOn), Some(Route::Axeos), "{v}");
        assert!(c.confirmed(MinerAction::LightOn), "{v} is hardware-confirmed");
        assert!(!c.supports(MinerAction::LightOff), "{v}: still no off command");
    }
    for v in ["v2.12.0", "v2.14.1", "v2.16.0", "v3.15.0"] {
        let c = caps_for_version_str(v);
        assert_eq!(c.route(MinerAction::LightOn), Some(Route::Axeos), "{v}");
        assert!(!c.confirmed(MinerAction::LightOn), "{v} stays dry-run only");
    }
    assert!(!caps_for_version_str("v2.11.0").supports(MinerAction::LightOn));
}

#[test]
fn light_policy_class() {
    use super::safety::ActionKind;
    for k in [ActionKind::LightOn, ActionKind::LightOff] {
        assert!(!k.irreversible() && !k.changes_config() && k.default_dry_run());
        assert_eq!(ActionKind::from(if k == ActionKind::LightOn { MinerAction::LightOn } else { MinerAction::LightOff }), k);
    }
    assert_eq!(serde_json::to_value(MinerAction::LightOn).unwrap(), "light_on");
}
