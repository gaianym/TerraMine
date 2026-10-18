//! Hardware check (ignored; run by hand against ONE Bitaxe on AxeOS >= 2.12):
//!   TM_HW_BITAXE_IP=<ip> cargo test --test hw_bitaxe_identify -- --ignored --nocapture
//! Sends "light on" once through TerraMine's real path (OverlayFactory ->
//! guarded job runner), which on AxeOS is the identify fallback
//! `POST /api/system/identify` (30 s on-screen "Hi!"). Nothing else is
//! sent: a second identify call toggles it, and there is no "off".
//! Afterwards the miner must still be hashing, not paused, and its uptime
//! must have kept counting (no reboot).
//!
//! First run: one Bitaxe Gamma (AxeOS v2.15.3, BM1370), Oct 17 2026. Route
//! axeos-http, POST answered 2xx (job status `unverified`: no read-back),
//! still hashing at 1030-1119 GH/s (baseline 1112), never paused, uptime
//! 33925 s -> 33970 s over 44 s (no reboot). This run is why identify is
//! marked hardware-confirmed for AxeOS 2.15.x.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use terra_mine_lib::control::driver::{DriverFactory, MinerAction, OverlayFactory, Route};
use terra_mine_lib::control::job::{run_action, ActionRowEvent, ActionStatus, JobConfig, VerifyConfig};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct Info {
    paused: Option<bool>,
    hashrate: Option<f64>,
    temp: Option<f64>,
    uptime: Option<u64>,
    version: Option<String>,
}

async fn info(c: &reqwest::Client, ip: IpAddr) -> anyhow::Result<Info> {
    let v: serde_json::Value = c.get(format!("http://{ip}/api/system/info")).send().await?.json().await?;
    Ok(Info {
        paused: v.get("miningPaused").and_then(|x| x.as_bool()),
        hashrate: v.get("hashRate").and_then(|x| x.as_f64()),
        temp: v.get("temp").and_then(|x| x.as_f64()),
        uptime: v.get("uptimeSeconds").and_then(|x| x.as_u64()),
        version: v.get("version").and_then(|x| x.as_str()).map(String::from),
    })
}

fn log(t0: Instant, what: &str, i: &Info) {
    println!(
        "[t+{:>4}s] {what}: version={:?} miningPaused={:?} hashRate={:?} GH/s temp={:?}°C uptime={:?}s",
        t0.elapsed().as_secs(),
        i.version,
        i.paused,
        i.hashrate.map(|h| (h * 10.0).round() / 10.0),
        i.temp,
        i.uptime
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real hardware: set TM_HW_BITAXE_IP"]
async fn hw_bitaxe_light_on_via_identify() {
    let ip: IpAddr = std::env::var("TM_HW_BITAXE_IP").expect("TM_HW_BITAXE_IP").parse().unwrap();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().unwrap();
    let t0 = Instant::now();
    let base = info(&http, ip).await.expect("baseline info");
    log(t0, "baseline", &base);
    assert_eq!(base.paused, Some(false), "must start hashing");
    let base_up = base.uptime.expect("baseline uptime");

    // Same factory the app uses; refuse to send unless light-on routes
    // through the AxeOS identify fallback.
    let factory: Arc<dyn DriverFactory> = Arc::new(OverlayFactory::new(Duration::from_secs(15)));
    let drv = factory.connect(ip).await.expect("connect").expect("miner found");
    let caps = drv.caps();
    println!("routes: {:?}", caps.routes());
    println!("light_on paths: {:?}", drv.paths_tried(MinerAction::LightOn));
    assert_eq!(caps.route(MinerAction::LightOn), Some(Route::Axeos), "light on must route via the AxeOS identify fallback");
    println!("light_on confirmed in caps: {}", caps.confirmed(MinerAction::LightOn));

    // Live send, with the unconfirmed route explicitly allowed for this test only.
    let cfg = JobConfig {
        action: MinerAction::LightOn,
        concurrency: 1,
        stagger: Duration::ZERO,
        timeout: Duration::from_secs(20),
        dry_run: false,
        verify: Some(VerifyConfig { interval: Duration::from_secs(2), timeout: Duration::from_secs(10) }),
        allow_unconfirmed: true,
    };
    let sink = Arc::new(move |e: ActionRowEvent| {
        println!("[t+{:>4}s]   event {:?} {:?} route={:?} msg={:?}", t0.elapsed().as_secs(), e.action, e.status, e.route, e.msg);
    });
    let sent_at = Instant::now();
    let r = run_action(vec![ip], cfg, factory, CancellationToken::new(), sink).await.remove(0);
    println!("[t+{:>4}s] final: {:?} route={:?} msg={:?}", t0.elapsed().as_secs(), r.status, r.route, r.msg);

    let mut problems = Vec::new();
    // Ok/Unverified: the POST returned HTTP 2xx; AxeOS cannot report the
    // identify state, so read-back ends as unverified by design.
    if !matches!(r.status, ActionStatus::Ok | ActionStatus::Unverified | ActionStatus::Verified) || r.route != Some(Route::Axeos) {
        problems.push(format!("identify: {:?} route {:?} {:?}", r.status, r.route, r.msg));
    }

    // Watch through the 30 s identify window and a bit beyond.
    let until = Instant::now() + Duration::from_secs(40);
    let mut last = None;
    while Instant::now() < until {
        tokio::time::sleep(Duration::from_secs(8)).await;
        if let Ok(i) = info(&http, ip).await {
            log(t0, "after", &i);
            last = Some(i);
        }
    }
    let Some(after) = last else { panic!("no /api/system/info after identify; {problems:?}") };
    if after.paused != Some(false) {
        problems.push(format!("miningPaused is {:?}", after.paused));
    }
    if !after.hashrate.is_some_and(|h| h > 0.0) {
        problems.push(format!("hashrate {:?}", after.hashrate));
    }
    let elapsed = sent_at.elapsed().as_secs();
    match after.uptime {
        Some(u) if u >= base_up => println!("uptime {base_up}s -> {u}s (+{}s over ~{elapsed}s): no reboot", u - base_up),
        other => problems.push(format!("uptime went from {base_up}s to {other:?}: rebooted?")),
    }
    assert!(problems.is_empty(), "{problems:?}");
}
