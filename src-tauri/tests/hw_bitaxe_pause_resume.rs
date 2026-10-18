//! Hardware check (ignored; run by hand against ONE Bitaxe on AxeOS >= 2.14):
//!   TM_HW_BITAXE_IP=<ip> cargo test --test hw_bitaxe_pause_resume -- --ignored --nocapture
//! Pause then resume through TerraMine's real path (OverlayFactory ->
//! guarded job runner), requiring the asic-rs route for both. Sends only
//! pause and resume; if resume does not verify, it falls back to the AxeOS
//! override `POST /api/system/resume` and fails the test.
//!
//! First run: one Bitaxe (AxeOS v2.15.3, BM1370), Oct 17 2026. Pause verified in
//! 3 s, resume verified in 2 s, hashrate back above 80% of baseline
//! after resume (about 20 s); no fallback used.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use terra_mine_lib::control::driver::{DriverFactory, MinerAction, OverlayFactory, Route};
use terra_mine_lib::control::job::{run_action, ActionRowEvent, ActionStatus, JobConfig, VerifyConfig};
use tokio_util::sync::CancellationToken;

struct Info {
    paused: Option<bool>,
    hashrate: Option<f64>,
    temp: Option<f64>,
    version: Option<String>,
}

async fn info(c: &reqwest::Client, ip: IpAddr) -> anyhow::Result<Info> {
    let v: serde_json::Value = c.get(format!("http://{ip}/api/system/info")).send().await?.json().await?;
    Ok(Info {
        paused: v.get("miningPaused").and_then(|x| x.as_bool()),
        hashrate: v.get("hashRate").and_then(|x| x.as_f64()),
        temp: v.get("temp").and_then(|x| x.as_f64()),
        version: v.get("version").and_then(|x| x.as_str()).map(String::from),
    })
}

fn log(t0: Instant, what: &str, i: &Info) {
    println!(
        "[t+{:>4}s] {what}: miningPaused={:?} hashRate={:?} GH/s temp={:?}°C",
        t0.elapsed().as_secs(),
        i.paused,
        i.hashrate.map(|h| (h * 10.0).round() / 10.0),
        i.temp
    );
}

fn cfg(action: MinerAction) -> JobConfig {
    JobConfig {
        action,
        concurrency: 1,
        stagger: Duration::ZERO,
        timeout: Duration::from_secs(20),
        dry_run: false,
        verify: Some(VerifyConfig { interval: Duration::from_secs(2), timeout: Duration::from_secs(60) }),
        allow_unconfirmed: false,
    }
}

async fn act(f: Arc<dyn DriverFactory>, ip: IpAddr, action: MinerAction, t0: Instant) -> ActionRowEvent {
    let events = Arc::new(Mutex::new(Vec::new()));
    let e2 = events.clone();
    let sink = Arc::new(move |e: ActionRowEvent| {
        println!("[t+{:>4}s]   event {:?} {:?} route={:?} msg={:?}", t0.elapsed().as_secs(), e.action, e.status, e.route, e.msg);
        e2.lock().unwrap().push(e);
    });
    run_action(vec![ip], cfg(action), f, CancellationToken::new(), sink).await.remove(0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real hardware: set TM_HW_BITAXE_IP"]
async fn hw_bitaxe_pause_resume_via_asic_rs() {
    let ip: IpAddr = std::env::var("TM_HW_BITAXE_IP").expect("TM_HW_BITAXE_IP").parse().unwrap();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let t0 = Instant::now();
    let base = info(&http, ip).await.expect("baseline info");
    println!("version {:?}", base.version);
    log(t0, "baseline", &base);
    assert_eq!(base.paused, Some(false), "must start hashing");
    let base_hr = base.hashrate.expect("baseline hashrate");

    // Same factory the app uses; refuse to send anything unless both
    // actions route through asic-rs.
    let factory: Arc<dyn DriverFactory> = Arc::new(OverlayFactory::new(Duration::from_secs(15)));
    let caps = factory.connect(ip).await.expect("connect").expect("miner found").caps();
    println!("routes: {:?}", caps.routes());
    assert_eq!(caps.route(MinerAction::Stop), Some(Route::AsicRs), "pause must route via asic-rs");
    assert_eq!(caps.route(MinerAction::Start), Some(Route::AsicRs), "resume must route via asic-rs");

    let stop = act(factory.clone(), ip, MinerAction::Stop, t0).await;
    let mut problems = Vec::new();
    if stop.status != ActionStatus::Verified || stop.route != Some(Route::AsicRs) {
        problems.push(format!("pause: {:?} route {:?} {:?}", stop.status, stop.route, stop.msg));
    }
    let paused_until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < paused_until {
        if let Ok(i) = info(&http, ip).await {
            log(t0, "paused", &i);
        }
        tokio::time::sleep(Duration::from_secs(6)).await;
    }

    let start = act(factory.clone(), ip, MinerAction::Start, t0).await;
    let resumed_ok = start.status == ActionStatus::Verified && start.route == Some(Route::AsicRs);
    if !resumed_ok {
        problems.push(format!("resume: {:?} route {:?} {:?}", start.status, start.route, start.msg));
        println!("[t+{:>4}s] FALLBACK: POST /api/system/resume (AxeOS override)", t0.elapsed().as_secs());
        let r = http.post(format!("http://{ip}/api/system/resume")).send().await;
        println!("  fallback result: {:?}", r.map(|r| r.status()));
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut recovered = false;
    while Instant::now() < deadline {
        if let Ok(i) = info(&http, ip).await {
            log(t0, "resumed", &i);
            if i.paused == Some(false) && i.hashrate.is_some_and(|h| h >= 0.8 * base_hr) {
                recovered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    if !recovered {
        problems.push(format!("hashrate did not reach 80% of {base_hr:.1} GH/s within 180 s"));
    }
    assert!(problems.is_empty(), "{problems:?}");
}
